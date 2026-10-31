use std::path::PathBuf;

use zorg::keys;
use zorg::network::WalletNetwork;
use zorg::sync;

const USAGE: &str = "\
zorg — a modern Zcash CLI wallet

USAGE:
    zorg [command] [options]

COMMANDS:
    accounts                                           List named accounts
    create [name]                                      Create an account with a new recovery phrase
    import [name] [--birthday <height>]                 Import a recovery phrase at account index 0
    balance [--account <name>]                         Show balance
    address [--account <name>]                         Show receive address
    sync [--account <name>]                            Sync all accounts, or the chosen account's database
    status [--account <name>]                          Show sync progress
    send <to> <amount> [--memo <text>] [--account <name>] Send ZEC
    history [--limit <n>] [--account <name>]             Show transaction history
    recovery [--account <name>]                        Show the recovery phrase
    validate <address>                                 Validate a Zcash address
    delete --account <name> [--yes]                     Delete an account

OPTIONS:
    --network <net>      main | test | regtest
    --lwd <url>          Lightwalletd endpoint
    --version, -V        Show version
    --help, -h           Show this help

ENV:
    ZORG_NETWORK         Network
    ZORG_LIGHTWALLETD_URL Lightwalletd endpoint
    RUST_LOG             Log level: error | warn | info | debug | trace
";

fn main() {
    zorg::init();
    let args: Vec<String> = std::env::args().collect();
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
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{USAGE}");
        return Ok(());
    }
    run_in(zorg::account::wallet_dir()?, args)
}

fn run_in(root: std::path::PathBuf, args: &[String]) -> Result<(), String> {
    let mut network = std::env::var("ZORG_NETWORK").ok();
    let mut lwd = std::env::var("ZORG_LIGHTWALLETD_URL").ok();
    let mut command_args = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--network" | "--lwd" => {
                let flag = &args[i];
                i += 1;
                let value = args
                    .get(i)
                    .ok_or_else(|| format!("{flag} requires a value"))?;
                if flag == "--network" {
                    network = Some(value.clone());
                } else {
                    lwd = Some(value.clone());
                }
            }
            _ => command_args.push(args[i].clone()),
        }
        i += 1;
    }
    let cmd = command_args
        .first()
        .map(String::as_str)
        .unwrap_or("accounts");
    let opts = command_args.get(1..).unwrap_or_default();
    if cmd == "validate" {
        let addr = opts.first().ok_or("Usage: zorg validate <address>")?;
        println!("Valid: {}", sync::validate_address(addr)?);
        return Ok(());
    }
    let network = network.as_deref().map(keys::parse_network).transpose()?;
    let index = if matches!(cmd, "create" | "import") {
        Some(zorg::account::open_index_for_write(&root)?)
    } else {
        None
    };
    let databases = match &index {
        Some(conn) => zorg::account::database_paths(conn)?,
        None => zorg::account::read_database_paths(&root)?,
    };
    let (accounts, unavailable) = zorg::account::accounts_from_databases(&databases)?;
    for message in &unavailable {
        eprintln!("{message}");
    }
    if matches!(cmd, "create" | "import") {
        zorg::account::require_available(&unavailable)?;
        let name = match positional_args(opts)?.first() {
            Some(name) => name.clone(),
            None => read_account_name()?,
        };
        zorg::account::ensure_unique_name(&accounts, &name)?;
        let net = network.unwrap_or(WalletNetwork::Main);
        let db_path = root.join(format!("{}.db", uuid::Uuid::new_v4()));
        let db = db_path.to_str().ok_or("Database path must be UTF-8")?;
        reserve_wallet_file(&db_path)?;
        let result = if cmd == "create" {
            let url = endpoint(lwd.as_deref(), &net, cmd);
            cmd_create(db, net, &url, &name)
        } else {
            cmd_import(db, net, opts, &name, &databases)
        };
        if let Err(e) = result {
            remove_wallet_files(db);
            return Err(e);
        }
        zorg::account::secure_wallet_db_file(&db_path)?;
        let index = index.ok_or("Database index is not open")?;
        return zorg::account::add_database_path(&index, &db_path)
            .and_then(|()| {
                index
                    .execute_batch("COMMIT")
                    .map_err(|e| format!("Could not update database index: {e}"))
            })
            .map_err(|e| {
                format!(
                    "Account created in {}; could not update index: {e}",
                    db_path.display()
                )
            });
    }
    let visible: Vec<_> = accounts
        .into_iter()
        .filter(|a| network.is_none_or(|n| n == a.network))
        .collect();
    if cmd == "accounts" {
        if visible.is_empty() {
            println!("No accounts found. Run `zorg create` or `zorg import`.");
        }
        for account in &visible {
            println!(
                "  {:<20} {:<7} {}",
                account.name,
                account.network.as_str(),
                account.unified_address
            );
        }
        return Ok(());
    }
    if !matches!(
        cmd,
        "balance" | "address" | "sync" | "status" | "send" | "history" | "recovery" | "delete"
    ) {
        return Err(format!("Unknown command: {cmd}"));
    }
    let name = account_name_opt(opts)?;
    if name.is_none() && !unavailable.is_empty() && !matches!(cmd, "sync" | "status") {
        return Err(
            "Some databases are unavailable; choose an account with --account <name>".into(),
        );
    }
    let selected = if matches!(cmd, "sync" | "status") && name.is_none() {
        if visible.is_empty() {
            return Err("No accounts found. Run `zorg create` or `zorg import`.".into());
        }
        None
    } else {
        Some(zorg::account::select(&visible, name.as_deref())?)
    };
    if matches!(cmd, "sync" | "status") {
        let targets: Vec<_> = selected
            .map(|a| vec![a])
            .unwrap_or_else(|| visible.iter().collect());
        let mut seen = std::collections::HashSet::new();
        for account in targets {
            if !seen.insert(&account.db_path) {
                continue;
            }
            let url = endpoint(lwd.as_deref(), &account.network, cmd);
            println!("{}:", account.name);
            if cmd == "sync" {
                sync::run_full_sync_blocking(&account.db_path, &url, account.network.as_str())?;
                println!("Sync complete.");
            } else {
                let p = sync::get_sync_progress(&account.db_path, account.network)?;
                let tip = zorg::sync_engine::fetch_chain_tip(&url).ok();
                let verified = zorg::sync_engine::last_completed_sync_at(&account.db_path)?;
                println!(
                    "{}",
                    render_sync_status(p.scanned_height, tip, verified, unix_now())
                );
            }
        }
        return Ok(());
    }
    let selected = selected.ok_or("Choose an account")?;
    let db = &selected.db_path;
    let net = selected.network;
    let uuid = &selected.uuid;
    let result = match cmd {
        "address" => {
            println!("{}", selected.unified_address);
            Ok(())
        }
        "balance" => cmd_balance(db, net, uuid),
        "send" => cmd_send(db, net, &endpoint(lwd.as_deref(), &net, cmd), opts, uuid),
        "history" => cmd_history(db, net, opts, uuid),
        "recovery" => cmd_recovery(net, selected),
        "delete" => {
            if name.is_none() {
                return Err("Usage: zorg delete --account <name> [--yes]".into());
            }
            cmd_delete(db, net, selected, opts)
        }
        _ => unreachable!(),
    };
    zorg::account::secure_wallet_db_file(std::path::Path::new(db))?;
    result
}

fn endpoint(explicit: Option<&str>, net: &WalletNetwork, cmd: &str) -> String {
    explicit
        .map(str::to_string)
        .unwrap_or_else(|| resolve_default_lwd_url(net, cmd))
}

fn read_account_name() -> Result<String, String> {
    use std::io::Write;
    print!("Account name: ");
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    let mut name = String::new();
    std::io::stdin()
        .read_line(&mut name)
        .map_err(|e| e.to_string())?;
    Ok(name.trim_end_matches(['\r', '\n']).to_string())
}

fn reserve_wallet_file(path: &std::path::Path) -> Result<(), String> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map(|_| ())
        .map_err(|e| format!("Could not create wallet database: {e}"))
}

fn cmd_create(db: &str, net: WalletNetwork, lwd_url: &str, name: &str) -> Result<(), String> {
    let birthday = resolve_create_birthday(|| zorg::sync_engine::get_latest_block_height(lwd_url))?;
    let mnemonic = keys::generate_mnemonic();
    let seed = keys::mnemonic_to_seed(&mnemonic)?;
    let created = zorg::account::init_db_and_create_account(db, net, &seed, Some(birthday), name)?;
    #[cfg(target_os = "macos")]
    store_and_verify_mnemonic(net, &created.uuid, &mnemonic, &seed)?;
    print!("{}", recovery_phrase_text(&mnemonic, birthday));
    println!("Account: {name}");
    println!("Address: {}", created.unified_address);
    Ok(())
}

/// Stores the mnemonic in the protected Keychain, then reads it back (which
/// asks for Touch ID or the login password) and checks it yields `seed`. On a
/// failed read-back the stored entry is removed.
#[cfg(target_os = "macos")]
fn store_and_verify_mnemonic(
    net: WalletNetwork,
    account_uuid: &str,
    mnemonic: &str,
    seed: &secrecy::SecretVec<u8>,
) -> Result<(), String> {
    use secrecy::ExposeSecret;

    zorg::secret_store::store_mnemonic_in_macos_keychain(net, account_uuid, mnemonic)
        .map_err(|e| format!("Failed to store mnemonic in keychain: {e}"))?;
    let verified = zorg::secret_store::seed_from_macos_stored_mnemonic(net, account_uuid)
        .map_err(|e| format!("Failed to read the mnemonic back from keychain: {e}"))
        .and_then(|stored| {
            if stored.expose_secret() == seed.expose_secret() {
                Ok(())
            } else {
                Err("Keychain returned a different mnemonic than the one just stored".into())
            }
        });
    if verified.is_err() {
        let _ = zorg::secret_store::delete_mnemonic_from_macos_keychain(net, account_uuid);
    }
    verified
}

/// Removes a wallet database and its SQLite side files, ignoring missing ones.
fn remove_wallet_files(db: &str) {
    for path in [db.to_string(), format!("{db}-wal"), format!("{db}-shm")] {
        let _ = std::fs::remove_file(path);
    }
}

/// The recovery phrase as numbered words, followed by the wallet birthday and
/// how to keep them.
fn recovery_phrase_text(mnemonic: &str, birthday: u64) -> String {
    let mut text = String::from("Recovery phrase (write it down and keep it offline):\n");
    let words: Vec<&str> = mnemonic.split_whitespace().collect();
    for (row, chunk) in words.chunks(4).enumerate() {
        let line: Vec<String> = chunk
            .iter()
            .enumerate()
            .map(|(col, word)| format!("{:>2}. {word:<8}", row * 4 + col + 1))
            .collect();
        text.push_str(&format!("  {}\n", line.join("  ").trim_end()));
    }
    text.push_str(&format!("Wallet birthday: {birthday}\n"));
    if cfg!(target_os = "macos") {
        text.push_str("A Touch ID-protected copy stays in this Mac's Keychain.\n");
    }
    text
}

fn resolve_create_birthday(fetch_tip: impl FnOnce() -> Result<u64, String>) -> Result<u64, String> {
    let tip = fetch_tip()
        .map_err(|e| format!("Could not determine wallet birthday from chain tip: {e}"))?;
    birthday_from_chain_tip(tip)
}

fn birthday_from_chain_tip(tip: u64) -> Result<u64, String> {
    if tip == 0 {
        return Err(
            "Could not determine wallet birthday: lightwalletd returned an empty chain".into(),
        );
    }
    Ok((tip.saturating_sub(100) / 100 * 100).max(1))
}

fn cmd_import(
    db: &str,
    net: WalletNetwork,
    opts: &[String],
    name: &str,
    databases: &[PathBuf],
) -> Result<(), String> {
    let birthday = flag_u64(opts, "--birthday");
    let mnemonic = read_mnemonic()?;
    let seed = keys::mnemonic_to_seed(&mnemonic)?;
    zorg::account::ensure_new_seed(databases, &seed, net)?;
    let created = zorg::account::init_db_and_create_account(db, net, &seed, birthday, name)?;
    #[cfg(target_os = "macos")]
    zorg::secret_store::store_mnemonic_in_macos_keychain(net, &created.uuid, &mnemonic)
        .map_err(|e| format!("Failed to store mnemonic in keychain: {e}"))?;

    println!("Account: {name}");
    println!("Address: {}", created.unified_address);
    Ok(())
}

fn cmd_delete(
    db: &str,
    net: WalletNetwork,
    account: &zorg::account::AccountInfo,
    opts: &[String],
) -> Result<(), String> {
    if !opts.iter().any(|a| a == "--yes") {
        println!(
            "This deletes account {:?} ({}).",
            account.name, account.unified_address
        );
        println!("Re-run with --yes to confirm.");
        return Ok(());
    }
    zorg::account::delete_account(db, net, &account.uuid)?;
    println!("Account {:?} deleted.", account.name);
    Ok(())
}

#[cfg(target_os = "macos")]
fn cmd_recovery(net: WalletNetwork, account: &zorg::account::AccountInfo) -> Result<(), String> {
    let mnemonic = zorg::secret_store::mnemonic_from_macos_keychain(net, &account.uuid)?;
    print!(
        "{}",
        recovery_phrase_text(&mnemonic, u64::from(account.birthday_height))
    );
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn cmd_recovery(_net: WalletNetwork, _account: &zorg::account::AccountInfo) -> Result<(), String> {
    Err("zorg recovery needs the macOS Keychain; on this platform zorg never stores the recovery phrase".into())
}

fn cmd_balance(db: &str, net: WalletNetwork, uuid: &str) -> Result<(), String> {
    let bal = sync::get_wallet_balance(db, net, uuid)?;
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

fn cmd_send(
    db: &str,
    net: WalletNetwork,
    lwd_url: &str,
    opts: &[String],
    uuid: &str,
) -> Result<(), String> {
    let positional = positional_args(opts)?;
    let to = positional
        .first()
        .ok_or("Usage: zorg send <to> <amount> [--memo <text>]")?;
    let amount_str = positional
        .get(1)
        .ok_or("Usage: zorg send <to> <amount> [--memo <text>]")?;
    let amount_zat = parse_zatoshi_amount(amount_str)?;
    let memo = flag_str(opts, "--memo");
    let send_flow_id = uuid::Uuid::new_v4().to_string();

    let proposal = sync::propose_send(
        db,
        net,
        uuid,
        &send_flow_id,
        to,
        amount_zat,
        memo.as_deref(),
    )?;
    println!("Fee: {}", fmt_zec(proposal.fee_zatoshi, net));

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(sync::execute_proposal_with_seed_loader(
        db,
        lwd_url,
        proposal.proposal_id,
        &send_flow_id,
        |network, account| load_seed(network, &account.expose_uuid().to_string()),
        None,
        None,
    ))?;
    for txid in &result.txids {
        println!("Broadcasted: {txid}");
    }
    Ok(())
}

fn cmd_history(db: &str, net: WalletNetwork, opts: &[String], uuid: &str) -> Result<(), String> {
    let limit = flag_u64(opts, "--limit").map(|n| n as u32);
    let txs = sync::get_transaction_history(db, net, limit, uuid)?;
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
            tx.txid
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

fn account_name_opt(opts: &[String]) -> Result<Option<String>, String> {
    if opts.iter().any(|a| a == "--account") {
        let name = flag_str(opts, "--account").ok_or("--account requires a name")?;
        zorg::account::validate_name(&name)?;
        Ok(Some(name))
    } else {
        Ok(None)
    }
}

fn positional_args(opts: &[String]) -> Result<Vec<String>, String> {
    let mut result = Vec::new();
    let mut i = 0;
    while i < opts.len() {
        match opts[i].as_str() {
            "--account" | "--memo" | "--limit" | "--birthday" => {
                i += 1;
                if i == opts.len() {
                    return Err(format!("{} requires a value", opts[i - 1]));
                }
            }
            "--yes" => {}
            value if value.starts_with("--") => return Err(format!("Unknown option: {value}")),
            value => result.push(value.to_string()),
        }
        i += 1;
    }
    Ok(result)
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

fn load_seed(net: WalletNetwork, account_uuid: &str) -> Result<secrecy::SecretVec<u8>, String> {
    #[cfg(target_os = "macos")]
    {
        zorg::secret_store::seed_from_macos_stored_mnemonic(net, account_uuid)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (net, account_uuid);
        let mnemonic = read_mnemonic()?;
        keys::mnemonic_to_seed(&mnemonic)
    }
}

/// Read the 24-word mnemonic from the terminal without echoing it.
fn read_mnemonic() -> Result<zeroize::Zeroizing<String>, String> {
    rpassword::prompt_password("Enter your 24-word recovery phrase: ")
        .map(zeroize::Zeroizing::new)
        .map_err(|e| format!("Failed to read mnemonic: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_database_paths(root: &std::path::Path, paths: &[PathBuf]) {
        let index = zorg::account::open_index_for_write(root).unwrap();
        for path in paths {
            zorg::account::add_database_path(&index, path).unwrap();
        }
        index.execute_batch("COMMIT").unwrap();
    }

    #[test]
    fn named_account_commands_use_the_owning_database_on_regtest() {
        let root = tempfile::tempdir().unwrap();
        let index_root = root.path().to_path_buf();
        zorg::account::ensure_private_wallet_dir(root.path()).unwrap();
        let seed = zorg::keys::mnemonic_to_seed(&zorg::keys::generate_mnemonic()).unwrap();
        let first_db = root.path().join("first.db");
        let first_db = first_db.to_str().unwrap();
        let first = zorg::account::init_db_and_create_account(
            first_db,
            WalletNetwork::Regtest,
            &seed,
            Some(1),
            "Daily",
        )
        .unwrap();
        let second = zorg::account::init_db_and_create_account(
            first_db,
            WalletNetwork::Regtest,
            &seed,
            Some(1),
            "Savings",
        )
        .unwrap();
        let other_seed = zorg::keys::mnemonic_to_seed(&zorg::keys::generate_mnemonic()).unwrap();
        let second_db = root.path().join("second.db");
        let second_db = second_db.to_str().unwrap();
        zorg::account::init_db_and_create_account(
            second_db,
            WalletNetwork::Regtest,
            &other_seed,
            Some(1),
            "Travel",
        )
        .unwrap();
        let command = |args: &[&str]| {
            let mut args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
            args.extend(["--network".into(), "regtest".into()]);
            super::run_in(index_root.clone(), &args)
        };
        set_database_paths(root.path(), &[first_db.into(), second_db.into()]);
        command(&[]).unwrap();
        assert!(command(&["balance"]).unwrap_err().contains("--account"));
        command(&["address", "--account", "Savings"]).unwrap();
        command(&["balance", "--account", "Travel"]).unwrap();
        command(&["history", "--account", "Daily"]).unwrap();
        command(&["delete", "--account", "Daily"]).unwrap();
        assert_eq!(
            zorg::account::list_accounts(first_db, WalletNetwork::Regtest)
                .unwrap()
                .len(),
            2
        );
        command(&["delete", "--account", "Daily", "--yes"]).unwrap();
        let remaining = zorg::account::list_accounts(first_db, WalletNetwork::Regtest).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].uuid, second.uuid);
        assert_ne!(remaining[0].uuid, first.uuid);
        assert_eq!(remaining[0].account_index, 1);
        command(&["balance", "--account", "Savings"]).unwrap();
        let offline = root.path().join("offline.db");
        std::fs::rename(second_db, &offline).unwrap();
        assert!(command(&["balance"]).unwrap_err().contains("unavailable"));
        command(&["balance", "--account", "Savings"]).unwrap();
        std::fs::rename(offline, second_db).unwrap();
    }

    #[test]
    fn send_options_are_not_mistaken_for_destination_or_amount() {
        let args: Vec<String> = [
            "--account",
            "Daily",
            "destination",
            "0.1",
            "--memo",
            "hello",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        assert_eq!(
            super::positional_args(&args).unwrap(),
            ["destination", "0.1"]
        );
        assert!(super::account_name_opt(&["--account".into()]).is_err());
    }

    #[test]
    fn first_run_lists_no_accounts_without_creating_a_database() {
        let root = tempfile::tempdir().unwrap();
        super::run_in(root.path().to_path_buf(), &[]).unwrap();
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn recovery_phrase_is_numbered_in_order_with_birthday() {
        let mnemonic = zorg::keys::generate_mnemonic();
        let text = recovery_phrase_text(&mnemonic, 4_562_700);
        let numbered: Vec<String> = text
            .lines()
            .filter(|line| line.starts_with("  "))
            .flat_map(|line| {
                line.split("  ")
                    .filter(|cell| cell.contains(". "))
                    .map(|cell| cell.trim().to_string())
                    .collect::<Vec<_>>()
            })
            .collect();
        let expected: Vec<String> = mnemonic
            .split_whitespace()
            .enumerate()
            .map(|(i, word)| format!("{}. {word}", i + 1))
            .collect();
        assert_eq!(numbered, expected);
        assert!(text.contains("Wallet birthday: 4562700"));
    }

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
    fn wallet_birthday_uses_chain_tip() {
        assert_eq!(resolve_create_birthday(|| Ok(1_000)).unwrap(), 900);
        assert_eq!(resolve_create_birthday(|| Ok(1_099)).unwrap(), 900);
        assert_eq!(
            resolve_create_birthday(|| Ok(4_562_637)).unwrap(),
            4_562_500
        );
        assert_eq!(resolve_create_birthday(|| Ok(199)).unwrap(), 1);
        assert_eq!(resolve_create_birthday(|| Ok(100)).unwrap(), 1);
        assert!(resolve_create_birthday(|| Ok(0)).is_err());
        assert!(resolve_create_birthday(|| Err("offline".into()))
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
