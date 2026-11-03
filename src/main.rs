use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::path::PathBuf;

mod terminal;

use zorg::keys;
use zorg::network::WalletNetwork;
use zorg::sync;

const USAGE: &str = "\
zorg — a modern Zcash CLI wallet

USAGE:
    zorg [command] [options]

COMMANDS:
    accounts                                                List named accounts
    create [name]                                           Create an account with a new recovery phrase
    import [name] [--birthday <height>]                     Import a recovery phrase at account index 0
    balance [--account <name>]                              Show balance
    address [--account <name>]                              Show receive address
    sync [--account <name>]                                 Sync all accounts, or the chosen account's database
    status [--account <name>]                               Show sync progress
    send <to> <amount> [--memo <text>] [--account <name>]   Send ZEC
    history [--limit <n>] [--account <name>]                Show transaction history
    recovery [--account <name>]                             Show the recovery phrase
    validate <address>                                      Validate a Zcash address
    delete --account <name>                                 Delete an account

OPTIONS:
    --network <net>      main | test | regtest
    --lwd <url>          Lightwalletd endpoint
    --version, -V        Show version
    --help, -h           Show this help

ENV:
    RUST_LOG             Log level: error | warn | info | debug | trace
";

fn main() {
    zorg::init();
    let args: Vec<String> = std::env::args().collect();
    let result = run(&args[1..]);
    if let Err(e) = result {
        if e == terminal::INTERRUPTED {
            std::process::exit(130);
        }
        if e == terminal::CANCELLED {
            terminal::note("Cancelled.");
            return;
        }
        if e != terminal::REPORTED {
            eprintln!("Error: {e}");
        }
        std::process::exit(1);
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let args = parse_args(args)?;
    if args.version {
        println!("zorg {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.help {
        print!("{USAGE}");
        return Ok(());
    }
    run_in(zorg::account::wallet_dir()?, args)
}

/// Options that take a value. `--network` and `--lwd` apply to every command.
const VALUE_OPTIONS: [&str; 6] = [
    "--network",
    "--lwd",
    "--account",
    "--memo",
    "--limit",
    "--birthday",
];

/// A command line parsed by [`parse_args`].
struct Args {
    command: String,
    positional: Vec<String>,
    options: HashMap<&'static str, String>,
    help: bool,
    version: bool,
}

/// The usage of `cmd`'s arguments, the command-specific options it accepts,
/// and how many positional arguments it takes.
fn command_spec(
    cmd: &str,
) -> Option<(&'static str, &'static [&'static str], RangeInclusive<usize>)> {
    Some(match cmd {
        "accounts" => ("", &[], 0..=0),
        "create" => (
            "<name>",
            &[],
            if terminal::interactive() {
                0..=1
            } else {
                1..=1
            },
        ),
        "import" => (
            "<name> [--birthday <height>]",
            &["--birthday"],
            if terminal::interactive() {
                0..=1
            } else {
                1..=1
            },
        ),
        "balance" | "address" | "sync" | "status" | "recovery" => {
            ("[--account <name>]", &["--account"], 0..=0)
        }
        "delete" => ("--account <name>", &["--account"], 0..=0),
        "send" => (
            "<to> <amount> [--memo <text>] [--account <name>]",
            &["--account", "--memo"],
            if terminal::interactive() {
                0..=2
            } else {
                2..=2
            },
        ),
        "history" => (
            "[--limit <n>] [--account <name>]",
            &["--account", "--limit"],
            0..=0,
        ),
        "validate" => ("<address>", &[], 0..=1),
        _ => return None,
    })
}

/// Parses the command line in one pass. Each value option takes the next
/// argument as its value. Fails on unknown or repeated options, options the
/// command does not accept, and a wrong number of positional arguments.
fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut words = Vec::new();
    let mut options = HashMap::new();
    let (mut help, mut version) = (false, false);
    let mut iter = args.iter().map(String::as_str);
    while let Some(arg) = iter.next() {
        if let Some(&option) = VALUE_OPTIONS.iter().find(|&&o| o == arg) {
            let value = iter
                .next()
                .ok_or_else(|| format!("{option} requires a value"))?;
            if options.insert(option, value.to_string()).is_some() {
                return Err(format!("{option} was given more than once"));
            }
        } else if arg == "--help" || arg == "-h" {
            help = true;
        } else if arg == "--version" || arg == "-V" {
            version = true;
        } else if arg.starts_with('-') && arg != "-" {
            return Err(format!("Unknown option: {arg}"));
        } else {
            words.push(arg.to_string());
        }
    }
    let mut words = words.into_iter();
    let command = words.next().unwrap_or_else(|| "accounts".into());
    let positional: Vec<String> = words.collect();
    if !(help || version) {
        let (usage, accepted, count) =
            command_spec(&command).ok_or_else(|| format!("Unknown command: {command}"))?;
        if let Some(option) = options
            .keys()
            .find(|o| !matches!(**o, "--network" | "--lwd") && !accepted.contains(o))
        {
            return Err(format!("{option} is not an option of zorg {command}"));
        }
        if !count.contains(&positional.len()) {
            let hint = if positional.len() > *count.end() {
                " (quote values that contain spaces)"
            } else {
                ""
            };
            let usage = format!("Usage: zorg {command} {usage}");
            return Err(format!("{}{hint}", usage.trim_end()));
        }
    }
    Ok(Args {
        command,
        positional,
        options,
        help,
        version,
    })
}

fn run_in(root: std::path::PathBuf, args: Args) -> Result<(), String> {
    let option = |name: &str| args.options.get(name).cloned();
    let network = option("--network");
    let lwd = option("--lwd");
    let cmd = args.command.as_str();
    if cmd == "validate" {
        let address = match args.positional.first() {
            Some(address) => address.clone(),
            None if terminal::interactive() => terminal::ask("Address", false, |_| Ok(()))?,
            None => return Err("Usage: zorg validate <address>".into()),
        };
        println!("Valid: {}", sync::validate_address(&address)?);
        return Ok(());
    }
    let network = network.as_deref().map(str::parse).transpose()?;
    let databases = zorg::account::read_database_paths(&root)?;
    let (accounts, unavailable) = zorg::account::accounts_from_databases(&databases)?;
    for message in &unavailable {
        eprintln!("{message}");
    }
    if matches!(cmd, "create" | "import") {
        zorg::account::require_available(&unavailable)?;
        let interactive = terminal::interactive();
        let name = match args.positional.first() {
            Some(name) => name.clone(),
            None => terminal::ask("Account name", false, |name| {
                zorg::account::ensure_unique_name(&accounts, name)
            })?,
        };
        zorg::account::ensure_unique_name(&accounts, &name)?;
        let mut birthday: Option<u64> = option("--birthday").and_then(|h| h.parse().ok());
        if cmd == "import" && birthday.is_none() && interactive {
            terminal::note("Scanning from the start takes hours on mainnet.");
            let height = terminal::ask(
                "Birthday height (Enter to scan from the start)",
                true,
                |h| match h.parse::<u32>() {
                    Ok(height) if height > 0 => Ok(()),
                    _ => Err("Enter a block height from 1 to 4294967295".to_string()),
                },
            )?;
            birthday = height.parse().ok();
        }
        let net = std::cell::Cell::new(network.unwrap_or(WalletNetwork::Main));
        if interactive {
            let (title, action, done) = if cmd == "create" {
                ("Create", "create", "created")
            } else {
                ("Import", "import", "imported")
            };
            let card = |_full: bool| {
                let mut lines = vec![
                    format!("{title} account {}", console::style(&name).bold()),
                    terminal::row("network", 8, net.get().display_name()),
                ];
                if cmd == "import" {
                    let from = birthday.map_or("the start".to_string(), |h| h.to_string());
                    lines.push(terminal::row("birthday", 8, from));
                }
                lines
            };
            let mut switch = || {
                let networks = WalletNetwork::ALL;
                let names = networks.map(|n| n.display_name().to_string());
                let current = networks.iter().position(|n| *n == net.get()).unwrap_or(0);
                net.set(networks[terminal::pick("Network", &names, current)?]);
                Ok(())
            };
            match terminal::confirm(action, 0, Some(("switch network", &mut switch)), card) {
                Ok(()) => {}
                Err(e) if e == terminal::CANCELLED => {
                    terminal::note(&format!("Cancelled. Nothing was {done}."));
                    return Ok(());
                }
                Err(e) => return Err(e),
            }
        }
        let net = net.get();
        zorg::account::ensure_private_wallet_dir(&root)?;
        let db_path = root.join(format!("{}.db", uuid::Uuid::new_v4()));
        let db = db_path.to_str().ok_or("Database path must be UTF-8")?;
        reserve_wallet_file(&db_path)?;
        let result = if cmd == "create" {
            endpoint(lwd.as_deref(), &net, cmd).and_then(|url| cmd_create(db, net, &url, &name))
        } else {
            cmd_import(db, net, birthday, &name, &databases)
        };
        let created = match result {
            Ok(created) => created,
            Err(e) => {
                let _ = zorg::account::remove_wallet_files(db);
                return Err(e);
            }
        };
        if let Err(e) = zorg::account::secure_wallet_db_file(&db_path).and_then(|()| {
            zorg::account::index_new_database(&root, &db_path, &name, &created.seed, net)
        }) {
            let _ = zorg::account::remove_wallet_files(db);
            let _ = zorg::secret_store::delete_mnemonic_from_macos_keychain(net, &created.uuid);
            return Err(e);
        }
        if let Some(birthday) = created.birthday {
            print!("{}", *recovery_phrase_text(&created.mnemonic, birthday));
        }
        println!("Account: {name}");
        println!("Address: {}", created.unified_address);
        return Ok(());
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
    let name = option("--account");
    let pick_account = terminal::interactive()
        && name.is_none()
        && visible.len() > 1
        && !matches!(cmd, "sync" | "status");
    if name.is_none()
        && !pick_account
        && !unavailable.is_empty()
        && !matches!(cmd, "sync" | "status")
    {
        return Err(
            "Some databases are unavailable; choose an account with --account <name>".into(),
        );
    }
    let selected = if matches!(cmd, "sync" | "status") && name.is_none() {
        if visible.is_empty() {
            return Err("No accounts found. Run `zorg create` or `zorg import`.".into());
        }
        None
    } else if pick_account {
        let items: Vec<String> = visible
            .iter()
            .map(|a| {
                let balance = sync::get_wallet_balance(&a.db_path, a.network, &a.uuid)
                    .map(|b| terminal::zec(b.total, 2))
                    .unwrap_or_else(|_| "?".into());
                format!("{:<20} {balance} {}", a.name, a.network.unit())
            })
            .collect();
        Some(&visible[terminal::pick("Which account?", &items, 0)?])
    } else {
        Some(zorg::account::select(&visible, name.as_deref())?)
    };
    if matches!(cmd, "sync" | "status") {
        let targets: Vec<_> = selected
            .map(|a| vec![a])
            .unwrap_or_else(|| visible.iter().collect());
        let missing = if selected.is_none() {
            unavailable.len()
        } else {
            0
        };
        let mut failed = missing;
        for account in &targets {
            println!("{}:", account.name);
            let result = endpoint(lwd.as_deref(), &account.network, cmd).and_then(|url| {
                if cmd == "sync" {
                    sync_database(&account.db_path, account.network, &url)
                } else {
                    print_database_status(&account.db_path, account.network, &url)
                }
            });
            if let Err(e) = result {
                eprintln!("Error: {e}");
                failed += 1;
            }
        }
        return if failed == 0 {
            Ok(())
        } else {
            Err(format!(
                "{failed} of {} databases failed",
                targets.len() + missing
            ))
        };
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
        "send" => {
            let memo = option("--memo");
            let missing = missing_phrase_error(selected);
            endpoint(lwd.as_deref(), &net, cmd).and_then(|url| {
                cmd_send(selected, &url, &args.positional, memo.as_deref(), &missing)
            })
        }
        "history" => {
            let limit = option("--limit").and_then(|n| n.parse::<u64>().ok());
            cmd_history(db, net, limit.map(|n| n as u32), uuid)
        }
        "recovery" => cmd_recovery(net, selected),
        "delete" => {
            if name.is_none() && !terminal::interactive() {
                return Err("Usage: zorg delete --account <name>".into());
            }
            cmd_delete(&root, selected)
        }
        _ => unreachable!(),
    };
    zorg::account::secure_wallet_db_file(std::path::Path::new(db))?;
    result
}

/// Syncs one wallet database with the chain.
fn sync_database(db_path: &str, net: WalletNetwork, lwd_url: &str) -> Result<(), String> {
    sync::run_full_sync_blocking(db_path, lwd_url, net)?;
    println!("Sync complete.");
    Ok(())
}

/// Prints one wallet database's sync progress against the chain tip.
fn print_database_status(db_path: &str, net: WalletNetwork, lwd_url: &str) -> Result<(), String> {
    let p = sync::get_sync_progress(db_path, net)?;
    let tip = zorg::sync_engine::fetch_chain_tip(lwd_url).ok();
    let verified = zorg::sync_engine::last_completed_sync_at(db_path)?;
    println!(
        "{}",
        render_sync_status(p.scanned_height, tip, verified, unix_now())
    );
    Ok(())
}

/// The lightwalletd endpoint for `net`: the explicit one, which must not serve
/// a different network, or a default one.
fn endpoint(explicit: Option<&str>, net: &WalletNetwork, cmd: &str) -> Result<String, String> {
    let Some(url) = explicit else {
        return Ok(resolve_default_lwd_url(net, cmd));
    };
    let height = zorg::sync_engine::get_lightwalletd_sapling_activation_height(url)?;
    let served = WalletNetwork::from_sapling_activation_height(height);
    if served != Some(*net) {
        let served = served.map_or("an unknown network".into(), |n| {
            format!("the {} network", n.as_str())
        });
        return Err(format!(
            "{url} serves {served}, but this database is on {}",
            net.as_str()
        ));
    }
    Ok(url.to_string())
}

fn reserve_wallet_file(path: &std::path::Path) -> Result<(), String> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
        .open(path)
        .map(|_| ())
        .map_err(|e| format!("Could not create wallet database: {e}"))
}

/// A created or imported account that is not yet in `zorg_index.db`.
struct NewAccount {
    uuid: String,
    unified_address: String,
    mnemonic: zeroize::Zeroizing<String>,
    seed: secrecy::SecretVec<u8>,
    /// The birthday to show with the recovery phrase; `None` for imports.
    birthday: Option<u64>,
}

fn cmd_create(
    db: &str,
    net: WalletNetwork,
    lwd_url: &str,
    name: &str,
) -> Result<NewAccount, String> {
    let birthday = resolve_create_birthday(|| zorg::sync_engine::get_latest_block_height(lwd_url))?;
    let mnemonic = keys::generate_mnemonic();
    let seed = keys::mnemonic_to_seed(&mnemonic)?;
    let created = zorg::account::init_db_and_create_account(db, net, &seed, Some(birthday), name)?;
    store_and_verify_mnemonic(net, &created.uuid, &mnemonic, &seed)?;
    Ok(NewAccount {
        uuid: created.uuid,
        unified_address: created.unified_address,
        mnemonic,
        seed,
        birthday: Some(birthday),
    })
}

/// Stores the mnemonic in the protected Keychain, then reads it back (which
/// asks for Touch ID or the login password) and checks it yields `seed`. On a
/// failed read-back the stored entry is removed.
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

/// The recovery phrase as numbered words, followed by the wallet birthday and
/// how to keep them.
fn recovery_phrase_text(mnemonic: &str, birthday: u64) -> zeroize::Zeroizing<String> {
    use std::fmt::Write;

    let words: Vec<&str> = mnemonic.split_whitespace().collect();
    let mut text = zeroize::Zeroizing::new(String::with_capacity(256 + 16 * words.len()));
    text.push_str("Recovery phrase (write it down and keep it offline):\n");
    for (row, chunk) in words.chunks(4).enumerate() {
        for (col, word) in chunk.iter().enumerate() {
            let number = row * 4 + col + 1;
            if col + 1 == chunk.len() {
                let _ = write!(text, "  {number:>2}. {word}");
            } else {
                let _ = write!(text, "  {number:>2}. {word:<8}");
            }
        }
        text.push('\n');
    }
    let _ = writeln!(text, "Wallet birthday: {birthday}");
    text.push_str("A Touch ID-protected copy stays in this Mac's Keychain.\n");
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
    birthday: Option<u64>,
    name: &str,
    databases: &[PathBuf],
) -> Result<NewAccount, String> {
    let mnemonic = read_mnemonic()?;
    let seed = keys::mnemonic_to_seed(&mnemonic)?;
    zorg::account::ensure_new_seed(databases, &seed, net)?;
    let created = zorg::account::init_db_and_create_account(db, net, &seed, birthday, name)?;
    store_and_verify_mnemonic(net, &created.uuid, &mnemonic, &seed)?;
    Ok(NewAccount {
        uuid: created.uuid,
        unified_address: created.unified_address,
        mnemonic,
        seed,
        birthday: None,
    })
}

fn cmd_delete(root: &std::path::Path, account: &zorg::account::AccountInfo) -> Result<(), String> {
    let interactive = terminal::interactive();
    if interactive {
        let unit = account.network.unit();
        let balance = sync::get_wallet_balance(&account.db_path, account.network, &account.uuid)
            .map_or("unknown".to_string(), |b| {
                format!("{} {unit}", terminal::zec(b.total, 2))
            });
        let card = |full: bool| {
            vec![
                format!("Delete account {}", console::style(&account.name).bold()),
                terminal::row(
                    "address",
                    7,
                    terminal::address(&account.unified_address, full),
                ),
                terminal::row("balance", 7, &balance),
                String::new(),
                format!(
                    "{} zorg can't show the recovery phrase again. Keep your written copy.",
                    console::style("!").yellow().bold()
                ),
            ]
        };
        match terminal::confirm("delete", account.unified_address.len(), None, card) {
            Ok(()) => {}
            Err(e) if e == terminal::CANCELLED => {
                terminal::note("Cancelled. Nothing was deleted.");
                return Ok(());
            }
            Err(e) => return Err(e),
        }
    } else {
        println!(
            "This deletes account {:?} ({}).",
            account.name, account.unified_address
        );
        println!("Keep the written-down recovery phrase: zorg can't show it again.");
    }
    zorg::secret_store::mnemonic_from_macos_keychain_if_present(account.network, &account.uuid)?;
    zorg::account::delete_account(root, account)?;
    zorg::secret_store::delete_mnemonic_from_macos_keychain(account.network, &account.uuid)?;
    if interactive {
        terminal::success(&format!("Deleted account {}", account.name));
    } else {
        println!("Account {:?} deleted.", account.name);
    }
    Ok(())
}

/// The error for an account whose recovery phrase is not in this Mac's
/// Keychain, with the commands that restore it.
fn missing_phrase_error(account: &zorg::account::AccountInfo) -> String {
    let network = match account.network {
        WalletNetwork::Main => String::new(),
        net => format!(" --network {}", net.as_str()),
    };
    format!(
        "The recovery phrase for {name:?} isn't in this Mac's Keychain \
         (Keychain entries don't move to a new Mac). Restore it with your written-down phrase:\n  \
         zorg delete --account {name:?}\n  \
         zorg import {name:?}{network} --birthday {birthday}",
        name = account.name,
        birthday = account.birthday_height,
    )
}

fn cmd_recovery(net: WalletNetwork, account: &zorg::account::AccountInfo) -> Result<(), String> {
    let mnemonic = zorg::secret_store::mnemonic_from_macos_keychain_if_present(net, &account.uuid)?
        .ok_or_else(|| missing_phrase_error(account))?;
    print!(
        "{}",
        *recovery_phrase_text(&mnemonic, u64::from(account.birthday_height))
    );
    Ok(())
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
    account: &zorg::account::AccountInfo,
    lwd_url: &str,
    positional: &[String],
    memo: Option<&str>,
    missing_phrase: &str,
) -> Result<(), String> {
    let (db, net, uuid) = (
        account.db_path.as_str(),
        account.network,
        account.uuid.as_str(),
    );
    let interactive = terminal::interactive();
    let unit = net.unit();
    let balance = sync::get_wallet_balance(db, net, uuid)?.spendable;
    let guided = positional.len() < 2;
    let to = match positional.first() {
        Some(to) => to.clone(),
        None => terminal::ask("To", false, |to| {
            zcash_keys::address::Address::decode(&net, to)
                .map(|_| ())
                .ok_or_else(|| format!("Not a valid {} address", net.as_str()))
        })?,
    };
    let to = to.as_str();
    let amount_zat = match positional.get(1) {
        Some(amount) => parse_zatoshi_amount(amount)?,
        None => {
            let amount = terminal::ask(&format!("Amount ({unit})"), false, |amount| {
                let zat = parse_zatoshi_amount(amount)?;
                if zat > balance {
                    return Err(format!(
                        "You have {} {unit} available",
                        terminal::zec(balance, 2)
                    ));
                }
                Ok(())
            })?;
            parse_zatoshi_amount(&amount)?
        }
    };
    let kind = sync::validate_address(to).unwrap_or_default();
    let asked_memo;
    let memo = match memo {
        None if guided && matches!(kind.as_str(), "unified" | "sapling") => {
            asked_memo = terminal::ask("Memo (optional)", true, |_| Ok(()))?;
            (!asked_memo.is_empty()).then_some(asked_memo.as_str())
        }
        memo => memo,
    };
    let proposal = sync::propose_send(db, net, uuid, to, amount_zat, memo)?;
    let fee = proposal.fee_zatoshi;
    let summary = send_summary(to, amount_zat, fee, memo, net);
    let card = |full: bool| {
        let total = amount_zat + fee;
        let mut lines = vec![
            format!(
                "Send {} {unit}   {}",
                console::style(terminal::zec(amount_zat, 0)).bold(),
                terminal::privacy_tag(&kind)
            ),
            terminal::row(
                "from",
                5,
                format!(
                    "{}  {} → {} {unit}",
                    account.name,
                    terminal::zec(balance, 2),
                    console::style(terminal::zec(balance.saturating_sub(total), 2)).bold()
                ),
            ),
            terminal::row("to", 5, terminal::address(to, full)),
            terminal::row("fee", 5, format!("{} {unit}", terminal::zec(fee, 0))),
            terminal::row("total", 5, format!("{} {unit}", terminal::zec(total, 0))),
        ];
        if let Some(memo) = memo {
            lines.push(terminal::row("memo", 5, format!("{memo:?}")));
        }
        lines
    };
    let progress = terminal::Progress::hidden();
    let broadcasting = std::cell::Cell::new(false);

    let rt = tokio::runtime::Runtime::new().unwrap();
    let result = rt.block_on(sync::execute_proposal_with_seed_loader(
        lwd_url,
        proposal,
        |network, account| {
            // A failed seed load releases the proposal's inputs, so cancelling here does too.
            if interactive {
                terminal::confirm("send", to.len(), None, card)?;
                progress.show("Waiting for Touch ID…");
            } else {
                print!("{summary}");
            }
            let seed = load_seed(network, &account.expose_uuid().to_string(), missing_phrase)?;
            if interactive {
                progress.show("Building the transaction…");
            }
            Ok(seed)
        },
        &|| {
            broadcasting.set(true);
            if interactive {
                progress.show("Broadcasting…");
            }
        },
    ));
    progress.clear();
    match result {
        Ok(result) if interactive => {
            terminal::success(&format!(
                "Sent {} {unit} to {}",
                terminal::zec(amount_zat, 0),
                terminal::short_address(to)
            ));
            for txid in &result.txids {
                println!("  {txid}");
            }
            terminal::note("  Pending · confirms in about a minute · zorg history");
            Ok(())
        }
        Ok(result) => {
            for txid in &result.txids {
                println!("Broadcasted: {txid}");
            }
            Ok(())
        }
        Err(e) if interactive && e == terminal::CANCELLED => {
            terminal::note("Cancelled. Nothing was sent.");
            Ok(())
        }
        Err(e) if interactive && e != terminal::INTERRUPTED => {
            if broadcasting.get() {
                terminal::failure(&format!(
                    "Broadcast failed: {e}. The transaction is saved and zorg retries it on \
                     the next sync — check zorg history before sending again."
                ));
            } else {
                terminal::failure(&format!("Not sent: {e}"));
            }
            Err(terminal::REPORTED.into())
        }
        Err(e) => Err(e),
    }
}

/// The amount, recipient, fee and memo of a send, shown before it is authorized.
fn send_summary(
    to: &str,
    amount_zat: u64,
    fee_zat: u64,
    memo: Option<&str>,
    net: WalletNetwork,
) -> String {
    let mut text = format!(
        "Send {} to\n  {to}\nFee: {}\n",
        fmt_zec(amount_zat, net),
        fmt_zec(fee_zat, net)
    );
    if let Some(memo) = memo {
        text.push_str(&format!("Memo: {memo:?}\n"));
    }
    text
}

fn cmd_history(db: &str, net: WalletNetwork, limit: Option<u32>, uuid: &str) -> Result<(), String> {
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

/// Testnet (and regtest) coins are worthless by design — say so.
fn fmt_zec(zatoshis: u64, net: WalletNetwork) -> String {
    format!("{:.8} {}", zatoshis as f64 / 100_000_000.0, net.unit())
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

/// Pick a lightwalletd endpoint when the user did not specify one.
fn resolve_default_lwd_url(net: &WalletNetwork, cmd: &str) -> String {
    let candidates = net.default_lightwalletd_urls();
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

/// The account's seed from Keychain; fails with `missing_phrase` when
/// Keychain has no entry for the account.
fn load_seed(
    net: WalletNetwork,
    account_uuid: &str,
    missing_phrase: &str,
) -> Result<secrecy::SecretVec<u8>, String> {
    let mnemonic = zorg::secret_store::mnemonic_from_macos_keychain_if_present(net, account_uuid)?
        .ok_or_else(|| missing_phrase.to_string())?;
    keys::mnemonic_to_seed(&mnemonic)
}

/// Read a 12- or 24-word mnemonic from the terminal without echoing it.
fn read_mnemonic() -> Result<zeroize::Zeroizing<String>, String> {
    rpassword::prompt_password("Enter your 12- or 24-word recovery phrase: ")
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
        let mut dbs = Vec::new();
        for name in ["Daily", "Savings", "Travel"] {
            let seed = zorg::keys::mnemonic_to_seed(&zorg::keys::generate_mnemonic()).unwrap();
            let db = root.path().join(format!("{name}.db"));
            zorg::account::init_db_and_create_account(
                db.to_str().unwrap(),
                WalletNetwork::Regtest,
                &seed,
                Some(1),
                name,
            )
            .unwrap();
            dbs.push(db);
        }
        let command = |args: &[&str]| {
            let mut args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
            args.extend(["--network".into(), "regtest".into()]);
            parse_args(&args).and_then(|args| super::run_in(index_root.clone(), args))
        };
        let account = |name: &str| {
            let db = root.path().join(format!("{name}.db"));
            zorg::account::list_accounts(db.to_str().unwrap(), WalletNetwork::Regtest)
                .unwrap()
                .remove(0)
        };
        set_database_paths(root.path(), &dbs);
        command(&[]).unwrap();
        assert!(command(&["balance"]).unwrap_err().contains("--account"));
        command(&["address", "--account", "Savings"]).unwrap();
        command(&["balance", "--account", "Travel"]).unwrap();
        command(&["history", "--account", "Daily"]).unwrap();

        zorg::account::delete_account(root.path(), &account("Daily")).unwrap();
        assert!(!dbs[0].exists());
        assert_eq!(
            zorg::account::read_database_paths(root.path()).unwrap(),
            dbs[1..]
        );

        let offline = root.path().join("offline.db");
        std::fs::rename(&dbs[2], &offline).unwrap();
        assert!(command(&["balance"]).unwrap_err().contains("unavailable"));
        assert_eq!(command(&["status"]).unwrap_err(), "1 of 2 databases failed");
        command(&["status", "--account", "Savings"]).unwrap();
        command(&["balance", "--account", "Savings"]).unwrap();
        std::fs::rename(&offline, &dbs[2]).unwrap();

        let mut travel = account("Travel");
        travel.uuid = account("Savings").uuid;
        assert!(zorg::account::delete_account(root.path(), &travel)
            .unwrap_err()
            .contains("not deleting"));
        assert!(dbs[2].exists());
        zorg::account::delete_account(root.path(), &account("Travel")).unwrap();
        assert!(!dbs[2].exists());
        assert_eq!(
            zorg::account::read_database_paths(root.path()).unwrap(),
            [dbs[1].clone()]
        );
    }

    #[test]
    fn options_take_their_values_and_are_checked_per_command() {
        let parse =
            |args: &[&str]| parse_args(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>());
        let args = parse(&[
            "send",
            "--account",
            "Daily",
            "to",
            "0.1",
            "--memo",
            "--account",
        ])
        .unwrap();
        assert_eq!(args.positional, ["to", "0.1"]);
        assert_eq!(args.options["--account"], "Daily");
        assert_eq!(args.options["--memo"], "--account");
        assert_eq!(
            parse(&["balance", "--account", "--help"]).unwrap().options["--account"],
            "--help"
        );
        assert_eq!(parse(&[]).unwrap().command, "accounts");
        assert!(parse(&["balance", "--account"]).is_err());
        assert!(parse(&["balance", "--account", "A", "--account", "B"]).is_err());
        assert!(parse(&["balance", "--acount", "A"]).is_err());
        assert!(parse(&["balance", "--memo", "hi"]).is_err());
        assert!(parse(&["sync", "Savings"]).is_err());
        assert!(parse(&["frobnicate"]).is_err());
        assert!(parse(&["create", "My", "Savings"])
            .err()
            .unwrap()
            .contains("quote"));
        assert!(parse(&["send", "--help"]).unwrap().help);
    }

    #[test]
    fn first_run_lists_no_accounts_without_creating_a_database() {
        let root = tempfile::tempdir().unwrap();
        super::run_in(root.path().to_path_buf(), parse_args(&[]).unwrap()).unwrap();
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn missing_phrase_error_shows_the_restore_commands() {
        let account = zorg::account::AccountInfo {
            uuid: String::new(),
            name: "My Savings".into(),
            unified_address: String::new(),
            account_index: 0,
            birthday_height: 2_345_600,
            db_path: String::new(),
            network: WalletNetwork::Test,
        };
        let error = missing_phrase_error(&account);
        assert!(
            error.contains("zorg delete --account \"My Savings\""),
            "{error}"
        );
        assert!(
            error.contains("zorg import \"My Savings\" --network test --birthday 2345600"),
            "{error}"
        );
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
    fn send_summary_shows_amount_recipient_fee_and_memo() {
        let summary = send_summary(
            "u1dest",
            150_000_000,
            10_000,
            Some("rent"),
            WalletNetwork::Main,
        );
        assert_eq!(
            summary,
            "Send 1.50000000 ZEC to\n  u1dest\nFee: 0.00010000 ZEC\nMemo: \"rent\"\n"
        );
        assert!(!send_summary("u1dest", 1, 10_000, None, WalletNetwork::Test).contains("Memo"));
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
