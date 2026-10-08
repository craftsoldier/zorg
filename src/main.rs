use rusqlite::OptionalExtension;
use zorg::keys;
use zorg::network::WalletNetwork;
use zorg::sync;

const USAGE: &str = "\
zorg — a modern Zcash CLI wallet

USAGE:
    zorg <command> [options]

COMMANDS:
    create [--birthday <height>]                        Create a wallet (default birthday: chain tip − 100)
    import <mnemonic> [--passphrase <p>]                Import from mnemonic
    accounts                                           List accounts (numbered)
    balance [--account <n>]                             Show balance
    address [--account <n>]                             Show receive address
    sync                                                Sync with the chain
    status                                              Show sync progress
    send <to> <amount> [--memo <text>] [--account <n>]  Send ZEC (TAZ on testnet)
    history [--limit <n>] [--account <n>]               Show transaction history
    validate <address>                                  Validate a Zcash address
    delete --account <n> [--yes]                        Delete an account (asks to confirm)

Accounts are numbered 1, 2, 3…; `zorg accounts` lists them. Commands that touch
a specific account take `--account <n>`; with one account you can omit it
(except `delete`, which always requires it).

OPTIONS:
    --db <path>          Wallet database path (default: ~/.zorg/wallet.db)
    --network <net>      main | test | regtest (default: wallet DB network, else main)
    --lwd <url>          Lightwalletd endpoint (default: zec.rocks with automatic failover across
                         trusted public endpoints; see DEFAULT_LWD_ENDPOINTS)
    --version, -V        Show version
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
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("zorg {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

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
    let db_path = std::path::Path::new(&db);
    let default_db = default_db_path();
    let default_wallet_dir = std::path::Path::new(&default_db)
        .parent()
        .map(std::path::Path::to_path_buf);
    let parent = db_path
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let parent = if parent.as_os_str().is_empty() {
        std::path::Path::new(".")
    } else {
        parent
    };
    if Some(parent) == default_wallet_dir.as_deref() {
        ensure_private_wallet_dir(parent)?;
    } else {
        ensure_custom_wallet_dir(parent)?;
    }
    secure_wallet_db_file(db_path)?;
    let cmd = command_args[0].as_str();
    let opts = &command_args[1..];
    let lwd_url = match lwd {
        // Explicit endpoint: the user's choice is used verbatim, no failover.
        Some(url) => url,
        None => resolve_default_lwd_url(&net, cmd),
    };

    let result = match cmd {
        "create" => cmd_create(&db, net, &lwd_url, opts),
        "import" => cmd_import(&db, net, opts),
        "accounts" => {
            let accounts = zorg::account::list_accounts(&db, net)?;
            if accounts.is_empty() {
                println!("No accounts yet. Run `zorg create` to make one.");
                return Ok(());
            }
            for a in &accounts {
                println!(
                    "  {:>2}  {:<20}  {}",
                    a.account_index + 1,
                    a.name,
                    a.unified_address
                );
            }
            Ok(())
        }
        "balance" => cmd_balance(&db, net, opts),
        "address" => {
            let uuid = zorg::account::resolve_account_uuid(&db, net, account_number_opt(opts)?)?;
            let addr = zorg::account::get_address_from_db(&db, net, &uuid)?;
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
            let live_tip = zorg::sync_engine::fetch_chain_tip(&lwd_url).ok();
            let verified_at = zorg::sync_engine::last_completed_sync_at(&db)?;
            println!(
                "{}",
                render_sync_status(p.scanned_height, live_tip, verified_at, unix_now())
            );
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
            let number =
                account_number_opt(opts)?.ok_or("Usage: zorg delete --account <n> [--yes]")?;
            let confirmed = opts.iter().any(|a| a == "--yes");
            let uuid = zorg::account::resolve_account_uuid(&db, net, Some(number))?;
            let address = zorg::account::get_address_from_db(&db, net, &uuid)?;
            if !confirmed {
                println!("This deletes Account {number} ({address}).");
                println!("Re-run with `zorg delete --account {number} --yes` to confirm.");
                return Ok(());
            }
            zorg::account::delete_account(&db, net, &uuid)?;
            println!("Account {number} deleted.");
            Ok(())
        }
        _ => {
            eprint!("{USAGE}");
            Err(format!("Unknown command: {cmd}"))
        }
    };

    secure_wallet_db_file(db_path)?;
    result
}

#[cfg(unix)]
fn ensure_private_wallet_dir(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|e| format!("Failed to create wallet directory: {e}"))?;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("Failed to secure wallet directory: {e}"))
}

#[cfg(unix)]
fn ensure_custom_wallet_dir(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|e| format!("Failed to create wallet directory: {e}"))?;

    let mode = std::fs::metadata(path)
        .map_err(|e| format!("Failed to inspect wallet directory: {e}"))?
        .permissions()
        .mode();
    if mode & 0o077 == 0 {
        Ok(())
    } else {
        Err(format!(
            "Wallet database directory {} is accessible by other users; use a private directory with permissions 0700",
            path.display()
        ))
    }
}

#[cfg(not(unix))]
fn ensure_private_wallet_dir(path: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|e| format!("Failed to create wallet directory: {e}"))
}

#[cfg(not(unix))]
fn ensure_custom_wallet_dir(path: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|e| format!("Failed to create wallet directory: {e}"))
}

#[cfg(unix)]
fn secure_wallet_db_file(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    if path.exists() {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("Failed to secure wallet database: {e}"))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn secure_wallet_db_file(_path: &std::path::Path) -> Result<(), String> {
    Ok(())
}

fn cmd_create(db: &str, net: WalletNetwork, lwd_url: &str, opts: &[String]) -> Result<(), String> {
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
    let result = keys::create_wallet(net.as_str(), db, Some(birthday))?;
    println!("Mnemonic (save this!): {}", result.mnemonic);
    println!("Account number: {}", result.account_number);
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
        .ok_or("Usage: zorg import <mnemonic> [--passphrase <p>]")?;
    let passphrase = flag_str(opts, "--passphrase").unwrap_or_default();
    let birthday = flag_u64(opts, "--birthday");
    let result = keys::import_wallet(mnemonic, &passphrase, birthday, net.as_str(), db)?;
    println!("Account number: {}", result.account_number);
    println!("Address: {}", result.unified_address);
    Ok(())
}

fn cmd_balance(db: &str, net: WalletNetwork, opts: &[String]) -> Result<(), String> {
    let uuid = zorg::account::resolve_account_uuid(db, net, account_number_opt(opts)?)?;
    let bal = sync::get_wallet_balance(db, net, &uuid)?;
    println!("Spendable:  {}", fmt_zec(bal.spendable, net));
    println!(
        "Pending:    {}",
        fmt_zec(bal.value_pending_spendability, net)
    );
    println!("Locked:     {}", fmt_zec(bal.locked, net));
    println!("Total:      {}", fmt_zec(bal.total, net));
    if bal.transparent > 0 {
        println!("  Transparent: {}", fmt_zec(bal.transparent, net));
    }
    if bal.sapling > 0 {
        println!("  Sapling:      {}", fmt_zec(bal.sapling, net));
    }
    if bal.orchard > 0 {
        println!("  Orchard:      {}", fmt_zec(bal.orchard, net));
    }
    Ok(())
}

fn cmd_send(db: &str, net: WalletNetwork, lwd_url: &str, opts: &[String]) -> Result<(), String> {
    let positional: Vec<&String> = opts.iter().filter(|a| !a.starts_with("--")).collect();
    let to = positional
        .first()
        .ok_or("Usage: zorg send <to> <amount> [--memo <text>]")?;
    let amount_str = positional
        .get(1)
        .ok_or("Usage: zorg send <to> <amount> [--memo <text>]")?;
    let amount_zat = parse_zatoshi_amount(amount_str)?;
    let memo = flag_str(opts, "--memo");
    let uuid = zorg::account::resolve_account_uuid(db, net, account_number_opt(opts)?)?;
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
    println!("Fee: {}", fmt_zec(proposal.fee_zatoshi, net));

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
    let uuid = zorg::account::resolve_account_uuid(db, net, account_number_opt(opts)?)?;
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
            fmt_zec(tx.display_amount, net),
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

/// `--account <n>` from the flags, parsed at the UI boundary. `None` = not given.
fn account_number_opt(opts: &[String]) -> Result<Option<u32>, String> {
    match flag_str(opts, "--account") {
        None => Ok(None),
        Some(raw) => match raw.parse::<u32>() {
            Ok(n) => Ok(Some(n)),
            Err(_) => {
                Err("Account must be a number (1, 2, 3…); `zorg accounts` lists them.".into())
            }
        },
    }
}

/// Testnet (and regtest) coins are worthless by design — say so.
fn fmt_zec(zatoshis: u64, net: WalletNetwork) -> String {
    let unit = match net {
        WalletNetwork::Main => "ZEC",
        WalletNetwork::Test | WalletNetwork::Regtest => "TAZ",
    };
    format!("{:.8} {unit}", zatoshis as f64 / 100_000_000.0)
}

/// Unix seconds since the epoch (0 if the clock is somehow before 1970).
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Human-readable age: "just now", "4m ago", "3h ago", "12d ago".
fn humanize_age(seconds_ago: u64) -> String {
    if seconds_ago < 90 {
        "just now".into()
    } else if seconds_ago < 3600 {
        format!("{}m ago", seconds_ago / 60)
    } else if seconds_ago < 86_400 {
        format!("{}h ago", seconds_ago / 3600)
    } else {
        format!("{}d ago", seconds_ago / 86_400)
    }
}

/// One honest status line: local scan position vs the live chain tip.
/// `live_tip` is None when the network was unreachable — say so rather
/// than let a locally-remembered tip claim completeness.
fn render_sync_status(
    scanned: u64,
    live_tip: Option<u64>,
    verified_at: Option<u64>,
    now: u64,
) -> String {
    let verified = verified_at
        .map(|t| humanize_age(now.saturating_sub(t)))
        .unwrap_or_else(|| "never".into());
    match live_tip {
        // Verified live right now; the timestamp only matters when behind.
        Some(tip) if scanned >= tip => {
            format!("fully synced ✓  height {scanned} (verified just now)")
        }
        Some(tip) => format!(
            "behind {behind} blocks  scanned {scanned} / chain {tip}  \
             (last verified {verified})\nrun: zorg sync",
            behind = tip - scanned,
        ),
        None if scanned == 0 => {
            format!("nothing scanned yet  (last verified {verified})\nrun: zorg sync")
        }
        None => format!(
            "offline — last known height {scanned}  \
             (not verified against the chain, last verified {verified})"
        ),
    }
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
        return Err("Invalid amount; use a positive decimal amount".into());
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

/// Candidate lightwalletd endpoints per network, in priority order.
fn default_lwd_urls(net: &WalletNetwork) -> &'static [&'static str] {
    match net {
        WalletNetwork::Main => &[
            "https://zec.rocks:443",
            "https://us.zec.stardust.rest:443",
            "https://eu.zec.stardust.rest:443",
            "https://eu.zec.rocks:443",
            "https://na.zec.rocks:443",
            "https://ap.zec.rocks:443",
            "https://sa.zec.rocks:443",
        ],
        WalletNetwork::Test => &["https://testnet.zec.rocks:443"],
        WalletNetwork::Regtest => &["http://localhost:9067"],
    }
}

/// Pick a lightwalletd endpoint when the user did not specify one.
fn resolve_default_lwd_url(net: &WalletNetwork, cmd: &str) -> String {
    let candidates = default_lwd_urls(net);
    let needs_network = matches!(cmd, "sync" | "create" | "send");
    let Some((first_alive, attempts)) = candidates.iter().enumerate().find_map(|(i, url)| {
        if !needs_network {
            return None;
        }
        match zorg::sync_engine::get_latest_block_height(url) {
            Ok(_) => Some((url, i + 1)),
            Err(e) => {
                eprintln!("note: {url} unreachable ({e}); trying next endpoint");
                None
            }
        }
    }) else {
        // No probe needed, or every candidate refused the connection: the
        // primary default is returned and the command itself reports failure.
        return candidates[0].to_string();
    };
    if attempts > 1 {
        eprintln!("note: failing over to {first_alive}");
    }
    first_alive.to_string()
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
    use super::{
        fmt_zec, parse_zatoshi_amount, render_sync_status, resolve_create_birthday, resolve_network,
    };
    use zorg::network::WalletNetwork;

    #[test]
    fn amounts_are_labeled_per_network() {
        assert_eq!(fmt_zec(29_000_000, WalletNetwork::Main), "0.29000000 ZEC");
        assert_eq!(fmt_zec(29_000_000, WalletNetwork::Test), "0.29000000 TAZ");
        assert_eq!(
            fmt_zec(29_000_000, WalletNetwork::Regtest),
            "0.29000000 TAZ"
        );
    }

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

    #[test]
    fn status_renders_fully_synced() {
        let s = render_sync_status(4_462_729, Some(4_462_729), Some(1_000), 2_000);
        assert_eq!(s, "fully synced ✓  height 4462729 (verified just now)");
        let s = render_sync_status(4_462_800, Some(4_462_729), None, 0);
        assert!(s.starts_with("fully synced ✓"), "{s}");
        assert!(s.contains("4462800"), "{s}");
    }

    #[test]
    fn status_renders_behind_with_nudge() {
        let s = render_sync_status(4_339_008, Some(4_462_634), Some(1_000), 2_000);
        assert!(s.starts_with("behind 123626 blocks"), "{s}");
        assert!(s.contains("scanned 4339008 / chain 4462634"), "{s}");
        assert!(s.contains("last verified 16m ago"), "{s}");
        assert!(s.contains("run: zorg sync"), "{s}");
    }

    #[test]
    fn status_renders_offline_without_claiming_completeness() {
        let s = render_sync_status(4_339_008, None, Some(120), 120 + 3 * 3600);
        assert!(s.starts_with("offline"), "{s}");
        assert!(s.contains("last known height 4339008"), "{s}");
        assert!(s.contains("not verified against the chain"), "{s}");
        assert!(s.contains("3h ago"), "{s}");
    }

    #[test]
    fn status_renders_never_synced_wallet() {
        let s = render_sync_status(0, None, None, 1_000);
        assert!(s.contains("nothing scanned yet"), "{s}");
        assert!(s.contains("last verified never"), "{s}");
        assert!(s.contains("run: zorg sync"), "{s}");
    }
}
