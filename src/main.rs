use std::collections::HashMap;
use std::ops::RangeInclusive;

mod terminal;

use zcash_client_backend::data_api::AccountBirthday;
use zcash_protocol::consensus::{NetworkUpgrade, Parameters};
use zorg::keys;
use zorg::network::WalletNetwork;
use zorg::sync;

/// The runtime every lightwalletd call runs on, for the life of the process.
static RUNTIME: std::sync::LazyLock<tokio::runtime::Runtime> =
    std::sync::LazyLock::new(|| tokio::runtime::Runtime::new().expect("tokio runtime"));

const BANNER: &str = include_str!("../.github/assets/zorg.txt");

const ESTIMATED_SYNC_BLOCKS_PER_SECOND: u64 = 200;

const USAGE: &str = "\
zorg — a modern Zcash CLI wallet

USAGE:
    zorg [command] [options]

COMMANDS:
    accounts                                                List named accounts
    create [name] --new [--network <net>]                   Create an account with a new recovery phrase
    create [name] --import [--birthday <height>]            Create an account from an existing recovery phrase
    balance [--account <name>]                              Show balance
    address [--new | --transparent] [--account <name>]      Show the receive address, make a new one, or show the transparent one
    address --parse <address>                               Show the receivers of a Zcash address
    sync [--account <name>]                                 Sync all accounts, or the chosen account's database
    status [--account <name>]                               Show sync progress
    send <to> <amount> [--memo <text>] [--account <name>]   Send ZEC
    send --shield [--account <name>]                        Move transparent funds into the shielded balance
    history [--limit <n>] [--account <name>]                Show transaction history
    tx <txid> [--account <name>]                            Show one transaction in full
    recovery [--account <name>]                             Show the recovery phrase
    delete --account <name>                                 Delete an account

OPTIONS:
    --network <net>      main | test | regtest (create)
    --lwd <url>          Lightwalletd endpoint
    --version, -V        Show version
    --help, -h           Show this help
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

/// Options that take a value. `--lwd` applies to every command.
const VALUE_OPTIONS: [&str; 7] = [
    "--network",
    "--lwd",
    "--account",
    "--memo",
    "--limit",
    "--birthday",
    "--parse",
];

/// Options that take no value.
const FLAG_OPTIONS: [&str; 4] = ["--new", "--import", "--transparent", "--shield"];

/// A command line parsed by [`parse_args`].
struct Args {
    /// The command, or none when only `zorg` was typed.
    command: Option<String>,
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
            "<name> (--new | --import [--birthday <height>]) [--network <net>]",
            &["--new", "--import", "--birthday", "--network"],
            if terminal::interactive() {
                0..=1
            } else {
                1..=1
            },
        ),
        "balance" | "sync" | "status" | "recovery" => ("[--account <name>]", &["--account"], 0..=0),
        "address" => (
            "[--new | --transparent] [--account <name>] | --parse <address>",
            &["--new", "--transparent", "--account", "--parse"],
            0..=0,
        ),
        "delete" => ("--account <name>", &["--account"], 0..=0),
        "send" => (
            "<to> <amount> [--memo <text>] [--account <name>]",
            &["--account", "--memo", "--shield"],
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
        "tx" => ("<txid> [--account <name>]", &["--account"], 1..=1),
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
        } else if let Some(&option) = FLAG_OPTIONS.iter().find(|&&o| o == arg) {
            if options.insert(option, String::new()).is_some() {
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
    let command = words.next();
    let positional: Vec<String> = words.collect();
    if !(help || version) {
        let command = command.as_deref().unwrap_or("accounts");
        let (usage, accepted, count) =
            command_spec(command).ok_or_else(|| format!("Unknown command: {command}"))?;
        if let Some(option) = options
            .keys()
            .find(|o| **o != "--lwd" && !accepted.contains(o))
        {
            return Err(format!("{option} is not an option of zorg {command}"));
        }
        let count = if options.contains_key("--shield") {
            0..=0
        } else {
            count
        };
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
    let lwd = option("--lwd");
    let cmd = args.command.as_deref().unwrap_or("accounts");
    if let Some(address) = option("--parse") {
        if ["--new", "--transparent", "--account"]
            .iter()
            .any(|name| args.options.contains_key(name))
        {
            return Err(
                "--parse cannot be combined with --new, --transparent, or --account".into(),
            );
        }
        for pool in address_receivers(&address)? {
            println!("{pool}");
        }
        return Ok(());
    }
    let accounts = zorg::account::read_index(&root)?;
    let saved = zorg::account::read_settings(&root)?;
    let server = |net: WalletNetwork| lwd.clone().or_else(|| saved.server(net));
    let mut unavailable = 0;
    for account in &accounts {
        if let Err(e) = zorg::account::ensure_database_exists(account) {
            eprintln!("{e}");
            unavailable += 1;
        }
    }
    if args.command.is_none() && accounts.is_empty() && terminal::interactive() {
        first_run();
        if create_account(&root, None, &args.options, &accounts, &server)?.is_none() {
            return Ok(());
        }
        println!();
        terminal::wait_for("Enter to open your account")?;
        return run_in(root, args);
    }
    if cmd == "create" {
        let name = args.positional.first().map(String::as_str);
        create_account(&root, name, &args.options, &accounts, &server)?;
        return Ok(());
    }
    let openable = |a: &&zorg::account::Account| zorg::account::ensure_database_exists(a).is_ok();
    let first = accounts
        .iter()
        .filter(openable)
        .find(|a| saved.default_account.as_deref() == Some(a.uuid.as_str()))
        .or_else(|| accounts.iter().find(openable));
    if let Some(account) = first.filter(|_| args.command.is_none() && terminal::interactive()) {
        if zorg::secret_store::mnemonic_if_present(
            &account.db_path,
            account.network,
            &account.uuid,
        )?
        .is_none()
        {
            terminal::failure(&format!(
                "No recovery phrase for {:?} on this Mac, so zorg cannot open it. \
                 Delete the account, then import it again from your written phrase.",
                account.name
            ));
            println!();
            cmd_delete(&root, account, saved.hide_balances)?;
            return if zorg::account::read_index(&root)?.len() < accounts.len() {
                run_in(root, args)
            } else {
                Ok(())
            };
        }
        let actions = ["Send", "Receive", "Transactions", "Settings", "Shield"].map(String::from);
        let mut hidden = saved.hide_balances;
        // Warnings would print over the screen; sync failures show in the sync row.
        log::set_max_level(log::LevelFilter::Off);
        let (send_update, sync_updates) = std::sync::mpsc::channel();
        {
            let (account, lwd) = (account.clone(), server(account.network));
            std::thread::spawn(move || RUNTIME.block_on(auto_sync(account, lwd, send_update)));
        }
        let mut sync_failure: Option<String> = None;
        loop {
            while let Ok(update) = sync_updates.try_recv() {
                sync_failure = update.err();
            }
            terminal::clear()?;
            let width = usize::from(console::Term::stdout().size().1);
            for line in BANNER.lines() {
                println!("{}", console::truncate_str(line, width, ""));
            }
            println!();
            let banner_height = BANNER.lines().count() + 1;
            let heading = format!("{} · {}", account.name, account.network.display_name());
            println!(
                "{}",
                console::style(console::truncate_str(&heading, width, "…")).bold()
            );
            let sync_row = |failure: &Option<String>| {
                let text = failure.clone().unwrap_or_else(|| sync_text(account));
                terminal::row("sync", 7, text)
            };
            println!("{}", balance_row(account, hidden));
            println!("{}\n", sync_row(&sync_failure));
            let balances = if hidden {
                "show balances"
            } else {
                "hide balances"
            };
            let mut repaint = || {
                let mut changed = false;
                while let Ok(update) = sync_updates.try_recv() {
                    sync_failure = update.err();
                    changed = true;
                }
                if changed {
                    terminal::paint_line(banner_height + 2, &balance_row(account, hidden));
                    terminal::paint_line(banner_height + 3, &sync_row(&sync_failure));
                }
            };
            let keys = [('b', balances), ('s', "settings")];
            let shieldable =
                sync::get_wallet_balance(&account.db_path, account.network, &account.uuid)
                    .is_ok_and(|b| {
                        b.transparent + b.transparent_pending >= sync::SHIELDING_THRESHOLD
                    });
            let items = if shieldable {
                vec![
                    actions[0].clone(),
                    actions[1].clone(),
                    actions[2].clone(),
                    actions[4].clone(),
                ]
            } else {
                actions[..3].to_vec()
            };
            let choice = terminal::menu_with_keys("q quit", &items, 0, &keys, &mut repaint);
            let choice = match choice? {
                None if hidden == saved.hide_balances => return Ok(()),
                None => return zorg::account::set_hide_balances(&root, hidden),
                Some(i) if i == items.len() => {
                    hidden = !hidden;
                    continue;
                }
                Some(i) if i == items.len() + 1 => 3,
                Some(3) => 4,
                Some(choice) => choice,
            };
            terminal::clear()?;
            println!("{}\n", console::style(&actions[choice]).bold());
            let result = match choice {
                0 => cmd_send(account, server(account.network).as_deref(), &[], None),
                1 => receive(account),
                2 => history(account, hidden),
                4 => cmd_shield(account, server(account.network).as_deref()),
                _ => settings(&root, account, &accounts, &saved, &args.options, hidden),
            };
            match result {
                Err(e) if e == terminal::INTERRUPTED => return Err(e),
                Err(e) if e == terminal::CANCELLED => terminal::note("Cancelled."),
                Err(e) if e == terminal::REPORTED => {}
                Err(e) => terminal::failure(&e),
                Ok(()) if !matches!(choice, 0 | 4) => continue,
                Ok(()) => {}
            }
            println!();
            terminal::wait_for("Esc back")?;
        }
    }
    if cmd == "accounts" {
        if accounts.is_empty() {
            println!("No accounts found. Run `zorg create`.");
        }
        for account in &accounts {
            let address =
                zorg::account::current_address(account).unwrap_or_else(|_| "(unavailable)".into());
            println!(
                "  {:<20} {:<7} {address}",
                account.name,
                account.network.as_str(),
            );
        }
        return Ok(());
    }
    let name = option("--account");
    let pick_account = terminal::interactive()
        && name.is_none()
        && accounts.len() > 1
        && !matches!(cmd, "sync" | "status");
    if name.is_none() && !pick_account && unavailable > 0 && !matches!(cmd, "sync" | "status") {
        return Err(
            "Some databases are unavailable; choose an account with --account <name>".into(),
        );
    }
    let selected = if matches!(cmd, "sync" | "status") && name.is_none() {
        if accounts.is_empty() {
            return Err("No accounts found. Run `zorg create`.".into());
        }
        None
    } else if pick_account {
        let items: Vec<String> = accounts
            .iter()
            .map(|a| {
                let balance = zorg::account::ensure_database_exists(a)
                    .and_then(|()| sync::get_wallet_balance(&a.db_path, a.network, &a.uuid))
                    .map(|b| terminal::zec(b.total, 2))
                    .unwrap_or_else(|_| "?".into());
                format!("{:<20} {balance} {}", a.name, a.network.unit())
            })
            .collect();
        Some(&accounts[terminal::pick("Which account?", &items, 0)?])
    } else {
        Some(zorg::account::select(&accounts, name.as_deref())?)
    };
    if let Some(account) = selected.filter(|_| cmd != "delete") {
        zorg::account::ensure_database_exists(account)?;
    }
    if matches!(cmd, "sync" | "status") {
        let targets: Vec<_> = selected
            .map(|a| vec![a])
            .unwrap_or_else(|| accounts.iter().collect());
        let mut failed = 0;
        for account in &targets {
            println!("{}:", account.name);
            let result = zorg::account::ensure_database_exists(account).and_then(|()| {
                let client = RUNTIME.block_on(zorg::sync_engine::connect_endpoint(
                    account.network,
                    server(account.network).as_deref(),
                ));
                if cmd == "sync" {
                    sync_database(&client?, &account.db_path, account.network)
                } else {
                    print_database_status(client.ok(), &account.db_path, account.network)
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
            Err(format!("{failed} of {} databases failed", targets.len()))
        };
    }
    let selected = selected.ok_or("Choose an account")?;
    let db = &selected.db_path;
    let net = selected.network;
    let uuid = &selected.uuid;
    let result = match cmd {
        "address" => {
            let address = if args.options.contains_key("--transparent") {
                if args.options.contains_key("--new") {
                    return Err("--new and --transparent cannot be used together".into());
                }
                zorg::account::transparent_address(selected)?
            } else if args.options.contains_key("--new") {
                sync::get_next_available_address(db, net, uuid)?
            } else {
                zorg::account::current_address(selected)?
            };
            println!("{address}");
            Ok(())
        }
        "balance" => cmd_balance(db, net, uuid),
        "send" if args.options.contains_key("--shield") => {
            if args.options.contains_key("--memo") {
                return Err("--memo cannot be used with --shield".into());
            }
            cmd_shield(selected, server(net).as_deref())
        }
        "send" => {
            let memo = option("--memo");
            cmd_send(
                selected,
                server(net).as_deref(),
                &args.positional,
                memo.as_deref(),
            )
        }
        "history" => {
            let limit = option("--limit")
                .map(|n| match n.parse::<u32>() {
                    Ok(0) => Err("--limit must be at least 1"),
                    Ok(n) => Ok(n),
                    Err(_) => Err("--limit must be a whole number"),
                })
                .transpose()?;
            cmd_history(db, net, limit, uuid)
        }
        "tx" => cmd_tx(selected, &args.positional[0], false),
        "recovery" => cmd_recovery(selected),
        "delete" => {
            if name.is_none() && !terminal::interactive() {
                return Err("Usage: zorg delete --account <name>".into());
            }
            cmd_delete(&root, selected, false)
        }
        _ => unreachable!(),
    };
    zorg::account::secure_wallet_db_file(std::path::Path::new(db))?;
    result
}

/// The account screen's balance row for `account`, as dots when `hide` is set.
fn balance_row(account: &zorg::account::Account, hide: bool) -> String {
    let balance = sync::get_wallet_balance(&account.db_path, account.network, &account.uuid)
        .map_or("unknown".to_string(), |b| {
            let unit = account.network.unit();
            let transparent = b.transparent + b.transparent_pending + b.transparent_locked;
            match (hide, transparent) {
                (true, _) => format!("•••• {unit}"),
                (false, 0) => format!("{} {unit}", terminal::zec(b.total, 2)),
                (false, t) => format!(
                    "{} {unit} · {} transparent",
                    terminal::zec(b.total, 2),
                    terminal::zec(t, 2)
                ),
            }
        });
    terminal::row("balance", 7, balance)
}

/// The account screen's sync row text: "synced", or a bar showing how
/// much of the wallet has been scanned.
fn sync_text(account: &zorg::account::Account) -> String {
    let synced =
        sync::get_sync_progress(&account.db_path, account.network).is_ok_and(|p| p.is_complete);
    match sync::scan_progress(&account.db_path, account.network) {
        _ if synced => "synced".into(),
        Ok(Some((scanned, total))) => {
            let done = (scanned as f64 / total as f64).min(0.99);
            format!("{} {:>2.0}%", terminal::bar(done, 24), done * 100.0)
        }
        _ => "syncing…".into(),
    }
}

/// Keeps `account` synced while the app is open: checks the chain tip every
/// 10 seconds and syncs when the wallet is behind, over one connection kept
/// between checks. A failed check drops the connection, and the next one
/// reconnects to the chosen server `lwd`, or to the first built-in server that
/// answers. It never draws: after each step it sends `Ok`, or `Err` with the
/// sync row's text when a sync fails. Runs until the process exits.
async fn auto_sync(
    account: zorg::account::Account,
    lwd: Option<String>,
    updates: std::sync::mpsc::Sender<Result<(), String>>,
) {
    let mut client = None;
    loop {
        let result = sync_pass(&account, lwd.as_deref(), &mut client, &updates).await;
        if result.is_err() {
            client = None;
        }
        let _ = updates.send(result.map_err(|e| {
            if e.starts_with("network:") {
                "server unreachable · retrying".to_string()
            } else {
                "failed · retrying".to_string()
            }
        }));
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }
}

/// One `auto_sync` check: connects when `client` is none, then syncs if the
/// chain tip is ahead of the wallet.
async fn sync_pass(
    account: &zorg::account::Account,
    lwd: Option<&str>,
    client: &mut Option<zorg::sync_engine::LwdClient>,
    updates: &std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    let client = match client {
        Some(client) => client,
        None => client.insert(zorg::sync_engine::connect_endpoint(account.network, lwd).await?),
    };
    let tip = zorg::sync_engine::chain_tip(client).await?;
    let progress = sync::get_sync_progress(&account.db_path, account.network)?;
    if progress.is_complete && tip <= progress.chain_tip_height {
        return Ok(());
    }
    let _ = updates.send(Ok(()));
    zorg::sync_engine::run_sync_inner(&account.db_path, client, account.network, |_| {
        let _ = updates.send(Ok(()));
    })
    .await
}

/// Syncs one wallet database with the chain.
fn sync_database(
    client: &zorg::sync_engine::LwdClient,
    db_path: &str,
    net: WalletNetwork,
) -> Result<(), String> {
    let progress = terminal::Progress::hidden_bar();
    let (stop, stopped) = std::sync::mpsc::channel::<()>();
    let result = std::thread::scope(|scope| {
        // The bar shows the same scan progress as the app's sync row, read about
        // once a second so the scan never waits on it.
        if terminal::interactive() {
            let progress = &progress;
            scope.spawn(move || loop {
                if let Ok(Some((done, total))) = sync::scan_progress(db_path, net) {
                    progress.set_progress(done, total);
                }
                if stopped.recv_timeout(std::time::Duration::from_secs(1))
                    != Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                {
                    break;
                }
            });
        }
        let result = RUNTIME.block_on(zorg::sync_engine::run_sync_inner(
            db_path,
            client,
            net,
            |_| {},
        ));
        drop(stop);
        result
    });
    progress.clear();
    result?;
    println!("Sync complete.");
    Ok(())
}

/// Prints one wallet database's sync progress against the chain tip.
fn print_database_status(
    client: Option<zorg::sync_engine::LwdClient>,
    db_path: &str,
    net: WalletNetwork,
) -> Result<(), String> {
    let p = sync::get_sync_progress(db_path, net)?;
    let tip = client.and_then(|mut client| {
        RUNTIME
            .block_on(zorg::sync_engine::chain_tip(&mut client))
            .ok()
    });
    let verified = zorg::sync_engine::last_completed_sync_at(db_path)?;
    println!(
        "{}",
        terminal::render_sync_status(p.scanned_height, tip, verified, terminal::unix_now())
    );
    Ok(())
}

/// The first-run screen: what zorg is.
fn first_run() {
    println!(
        "{}",
        console::style("zorg · a Zcash wallet for your terminal").bold()
    );
    terminal::note("Recovery phrases stay on this Mac, behind macOS authentication.");
    terminal::note("Run zorg --help to see every command.");
    println!();
}

/// Creates an account named `name` (asked for when none), with a new recovery
/// phrase (`--new`) or an imported one (`--import`), asked for in a terminal
/// when neither is given. The new account becomes the default. Returns it, or
/// none when the person cancelled.
fn create_account(
    root: &std::path::Path,
    name: Option<&str>,
    options: &HashMap<&'static str, String>,
    accounts: &[zorg::account::Account],
    server: &dyn Fn(WalletNetwork) -> Option<String>,
) -> Result<Option<zorg::account::Account>, String> {
    let interactive = terminal::interactive();
    let mut net = options
        .get("--network")
        .map(|n| n.parse())
        .transpose()?
        .unwrap_or(WalletNetwork::Main);
    let import = match (
        options.contains_key("--new"),
        options.contains_key("--import"),
    ) {
        (true, true) => return Err("Choose either --new or --import, not both".into()),
        (new, import) if new || import => import,
        _ if interactive => {
            let ways = ["Import an existing account", "Start a new account"].map(String::from);
            loop {
                let key = format!("network: {}", net.display_name());
                match terminal::menu_with_keys("Esc cancel", &ways, 0, &[('n', &key)], &mut || {})?
                {
                    Some(i) if i == ways.len() => net = net.next(),
                    Some(choice) => {
                        terminal::chose("Create account", &ways[choice]);
                        break choice == 0;
                    }
                    None => {
                        terminal::note("Cancelled. Nothing was created.");
                        return Ok(None);
                    }
                }
            }
        }
        _ => return Err("Choose --new for a new recovery phrase or --import to import one".into()),
    };
    let mut birthday = options
        .get("--birthday")
        .map(|h| {
            h.parse::<u32>()
                .map(u64::from)
                .map_err(|_| "--birthday must be a block height from 0 to 4294967295")
        })
        .transpose()?;
    if birthday.is_some() && !import {
        return Err("--birthday only applies with --import".into());
    }
    let imported = if import {
        Some(terminal::read_mnemonic(net, |mnemonic| {
            let seed = keys::mnemonic_to_seed(mnemonic)?;
            zorg::account::ensure_new_seed(accounts, &seed, net)?;
            Ok(seed)
        })?)
    } else {
        None
    };
    let tip = if import && interactive {
        let mut client = RUNTIME.block_on(zorg::sync_engine::connect_endpoint(
            net,
            server(net).as_deref(),
        ))?;
        Some(RUNTIME.block_on(zorg::sync_engine::chain_tip(&mut client))?)
    } else {
        None
    };
    if import && birthday.is_none() && interactive {
        terminal::note("Wallet birthday is the block height when this wallet was created.");
        let height = terminal::ask(
            "Birthday height (Enter to scan from the start)",
            true,
            |h| {
                let height = h
                    .parse::<u32>()
                    .map(u64::from)
                    .map_err(|_| "Enter a block height from 0 to 4294967295".to_string())?;
                if let Some(tip) = tip.filter(|tip| height > *tip) {
                    return Err(format!("Birthday is above the current height {tip}"));
                }
                Ok(())
            },
        )?;
        birthday = height.parse::<u64>().ok();
    }
    if let (Some(birthday), Some(tip)) = (birthday, tip) {
        if birthday > tip {
            return Err(format!(
                "Birthday {birthday} is above {}'s current height {tip}",
                net.display_name()
            ));
        }
    }
    let name = match name {
        Some(name) => name.to_string(),
        None => terminal::ask("Account name", false, |name| {
            zorg::account::ensure_unique_name(accounts, name)
        })?,
    };
    zorg::account::ensure_unique_name(accounts, &name)?;
    let estimate = tip.map(|tip| {
        let start = birthday.unwrap_or_else(|| {
            u64::from(u32::from(
                net.activation_height(NetworkUpgrade::Sapling)
                    .expect("supported networks activate Sapling"),
            ))
        });
        terminal::estimated_sync(
            tip.saturating_sub(start)
                .div_ceil(ESTIMATED_SYNC_BLOCKS_PER_SECOND),
        )
    });
    if interactive {
        let (title, action, done) = if import {
            ("Import", "import", "imported")
        } else {
            ("Create", "create", "created")
        };
        let card = |_full: bool| {
            let mut lines = vec![
                format!("{title} account {}", console::style(&name).bold()),
                terminal::row("network", if import { 14 } else { 8 }, net.display_name()),
            ];
            if import {
                let from = birthday.map_or("scan from the start".to_string(), |h| h.to_string());
                lines.push(terminal::row("birthday", 14, from));
                if let Some(estimate) = &estimate {
                    lines.push(terminal::row("estimated sync", 14, estimate));
                }
            }
            lines
        };
        match terminal::confirm(action, 0, card) {
            Ok(()) => {}
            Err(e) if e == terminal::CANCELLED => {
                terminal::note(&format!("Cancelled. Nothing was {done}."));
                return Ok(None);
            }
            Err(e) => return Err(e),
        }
    }
    let mut account_birthday = None;
    if let Some(birthday) = birthday {
        let mut client = RUNTIME.block_on(zorg::sync_engine::connect_endpoint(
            net,
            server(net).as_deref(),
        ))?;
        let tip = match tip {
            Some(tip) => tip,
            None => RUNTIME.block_on(zorg::sync_engine::chain_tip(&mut client))?,
        };
        if birthday > tip {
            return Err(format!(
                "Birthday {birthday} is above {}'s current height {tip}",
                net.display_name()
            ));
        }
        account_birthday = Some(RUNTIME.block_on(zorg::sync_engine::fetch_birthday(
            &mut client,
            net,
            birthday,
        ))?);
    }
    zorg::account::ensure_private_wallet_dir(root)?;
    let db_path = root.join(format!("{}.db", uuid::Uuid::new_v4()));
    let db = db_path.to_str().ok_or("Database path must be UTF-8")?;
    reserve_wallet_file(&db_path)?;
    let result = if let Some((mnemonic, seed)) = imported {
        cmd_import(db, net, account_birthday, &name, mnemonic, seed)
    } else {
        cmd_create(db, net, server(net).as_deref(), &name)
    };
    let created = match result {
        Ok(created) => created,
        Err(e) => {
            let _ = zorg::account::remove_wallet_files(db);
            return Err(e);
        }
    };
    let new_account = zorg::account::Account {
        uuid: created.uuid.clone(),
        name: name.clone(),
        network: net,
        db_path: db.to_string(),
    };
    if let Err(e) = zorg::account::secure_wallet_db_file(&db_path)
        .and_then(|()| zorg::account::index_new_account(root, &new_account, &created.seed))
    {
        let _ = zorg::account::remove_wallet_files(db);
        let _ = zorg::secret_store::delete_mnemonic(db, net, &created.uuid);
        return Err(e);
    }
    if let Some(birthday) = created.birthday {
        print!(
            "{}",
            *terminal::recovery_phrase_text(&created.mnemonic, birthday)
        );
    }
    println!("Account: {name}");
    println!("Address: {}", created.unified_address);
    Ok(Some(new_account))
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

fn cmd_create(
    db: &str,
    net: WalletNetwork,
    server: Option<&str>,
    name: &str,
) -> Result<zorg::account::NewAccount, String> {
    let mut client = RUNTIME.block_on(zorg::sync_engine::connect_endpoint(net, server))?;
    let birthday =
        resolve_create_birthday(|| RUNTIME.block_on(zorg::sync_engine::chain_tip(&mut client)))?;
    let account_birthday = RUNTIME.block_on(zorg::sync_engine::fetch_birthday(
        &mut client,
        net,
        birthday,
    ))?;
    let mnemonic = keys::generate_mnemonic();
    let seed = keys::mnemonic_to_seed(&mnemonic)?;
    let created =
        zorg::account::init_db_and_create_account(db, net, &seed, Some(account_birthday), name)?;
    store_and_verify_mnemonic(db, net, &created.uuid, &mnemonic)?;
    Ok(zorg::account::NewAccount {
        uuid: created.uuid,
        unified_address: created.unified_address,
        mnemonic,
        seed,
        birthday: Some(birthday),
    })
}

/// Stores the mnemonic and checks an authenticated read-back against the input.
/// On a failed read-back the stored entry is removed.
fn store_and_verify_mnemonic(
    db: &str,
    net: WalletNetwork,
    account_uuid: &str,
    mnemonic: &str,
) -> Result<(), String> {
    zorg::secret_store::store_mnemonic(db, net, account_uuid, mnemonic)
        .map_err(|e| format!("Failed to store mnemonic: {e}"))?;
    let verified = zorg::secret_store::mnemonic(db, net, account_uuid)
        .map_err(|e| format!("Failed to read the mnemonic back: {e}"))
        .and_then(|stored| {
            if stored.as_str() == mnemonic {
                Ok(())
            } else {
                Err("The store returned a different mnemonic than the one just stored".into())
            }
        });
    if verified.is_err() {
        let _ = zorg::secret_store::delete_mnemonic(db, net, account_uuid);
    }
    verified
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
    birthday: Option<AccountBirthday>,
    name: &str,
    mnemonic: zeroize::Zeroizing<String>,
    seed: secrecy::SecretVec<u8>,
) -> Result<zorg::account::NewAccount, String> {
    let created = zorg::account::init_db_and_create_account(db, net, &seed, birthday, name)?;
    store_and_verify_mnemonic(db, net, &created.uuid, &mnemonic)?;
    Ok(zorg::account::NewAccount {
        uuid: created.uuid,
        unified_address: created.unified_address,
        mnemonic,
        seed,
        birthday: None,
    })
}

/// The Receive screen: `account`'s full address, with Copy address and New
/// address, until Esc goes back.
fn receive(account: &zorg::account::Account) -> Result<(), String> {
    let mut address = zorg::account::current_address(account)?;
    let mut status: Result<String, String> = Ok(String::new());
    let mut transparent: Option<String> = None;
    loop {
        terminal::clear()?;
        println!("{}\n", console::style("Receive").bold());
        println!("{}\n", transparent.as_ref().unwrap_or(&address));
        match &status {
            Ok(message) if message.is_empty() => {}
            Ok(message) => terminal::note(&format!("{message}\n")),
            Err(e) => terminal::failure(&format!("{e}\n")),
        }
        let (actions, key) = match transparent {
            Some(_) => (&["Copy address".to_string()][..], ('t', "shielded")),
            None => (
                &["Copy address", "New address"].map(String::from)[..],
                ('t', "transparent"),
            ),
        };
        let choice = terminal::menu_with_keys("Esc back", actions, 0, &[key], &mut || {})?;
        status = match choice {
            None => return Ok(()),
            Some(i) if i == actions.len() => {
                transparent = match transparent {
                    Some(_) => None,
                    None => match zorg::account::transparent_address(account) {
                        Ok(t) => Some(t),
                        Err(e) => {
                            status = Err(e);
                            continue;
                        }
                    },
                };
                Ok(String::new())
            }
            Some(0) => {
                terminal::copy(transparent.as_ref().unwrap_or(&address)).map(|()| "Copied.".into())
            }
            Some(_) => {
                sync::get_next_available_address(&account.db_path, account.network, &account.uuid)
                    .map(|new| {
                        address = new;
                        "New address.".into()
                    })
            }
        };
    }
}

/// Starts zorg over with the same arguments, so the app opens with the
/// settings just saved. Returns only if that fails.
fn reopen() -> String {
    use std::os::unix::process::CommandExt;
    match std::env::current_exe() {
        Ok(exe) => {
            let error = std::process::Command::new(exe)
                .args(std::env::args().skip(1))
                .exec();
            format!("Could not reopen zorg: {error}")
        }
        Err(e) => format!("Could not reopen zorg: {e}"),
    }
}

/// The Settings screen: accounts, the server for `account`'s network, its
/// viewing key and recovery phrase, until Esc goes back. A saved change reopens zorg.
fn settings(
    root: &std::path::Path,
    account: &zorg::account::Account,
    accounts: &[zorg::account::Account],
    saved: &zorg::account::Settings,
    options: &HashMap<&'static str, String>,
    hide: bool,
) -> Result<(), String> {
    let server = saved
        .server(account.network)
        .unwrap_or_else(|| "built-in".into());
    let items = [
        format!("{:<11} {}", "Accounts", account.name),
        format!("{:<11} {server}", "Server"),
        "Viewing key".to_string(),
        "Recovery phrase".to_string(),
    ];
    let mut current = 0;
    loop {
        terminal::clear()?;
        println!("{}\n", console::style("Settings").bold());
        let Some(choice) = terminal::menu("Esc back", &items, current)? else {
            return Ok(());
        };
        current = choice;
        let result = match choice {
            0 => switch_account(root, account, accounts, options, saved, hide),
            1 => choose_server(root, account.network),
            2 => show_viewing_key(account).map(|()| false),
            _ => show_recovery_phrase(account).map(|()| false),
        };
        match result {
            Ok(true) => {
                if hide != saved.hide_balances {
                    zorg::account::set_hide_balances(root, hide)?;
                }
                return Err(reopen());
            }
            Ok(false) => {}
            Err(e) if e == terminal::INTERRUPTED => return Err(e),
            Err(e) if e == terminal::CANCELLED => {}
            Err(e) => {
                terminal::failure(&e);
                println!();
                terminal::wait_for("Esc back")?;
            }
        }
    }
}

/// Settings → Accounts: open another account, add one, or delete one other
/// than the open `account`, with balances hidden as `saved` says. Returns
/// whether anything changed.
fn switch_account(
    root: &std::path::Path,
    account: &zorg::account::Account,
    accounts: &[zorg::account::Account],
    options: &HashMap<&'static str, String>,
    saved: &zorg::account::Settings,
    hide: bool,
) -> Result<bool, String> {
    terminal::clear()?;
    println!("{}\n", console::style("Accounts").bold());
    let mut items: Vec<String> = accounts
        .iter()
        .map(|a| {
            let open = if a.uuid == account.uuid {
                "  (open)"
            } else {
                ""
            };
            format!("{:<20} {}{open}", a.name, a.network.display_name())
        })
        .collect();
    items.push("Add account".into());
    let current = accounts
        .iter()
        .position(|a| a.uuid == account.uuid)
        .unwrap_or(0);
    let Some(choice) = terminal::menu("Esc back", &items, current)? else {
        return Ok(false);
    };
    let Some(picked) = accounts.get(choice) else {
        terminal::clear()?;
        let lwd = options.get("--lwd").cloned();
        let server = |net| lwd.clone().or_else(|| saved.server(net));
        if create_account(root, None, options, accounts, &server)?.is_none() {
            println!();
            terminal::wait_for("Esc back")?;
            return Ok(false);
        }
        println!();
        terminal::wait_for("Enter to open your account")?;
        return Ok(true);
    };
    if picked.uuid == account.uuid {
        return Ok(false);
    }
    let actions = ["Open account", "Delete account"].map(String::from);
    terminal::clear()?;
    println!("{}\n", console::style(&picked.name).bold());
    let Some(action) = terminal::menu("Esc back", &actions, 0)? else {
        return Ok(false);
    };
    if action == 0 {
        zorg::account::set_default_account(root, &picked.uuid)?;
        return Ok(true);
    }
    terminal::clear()?;
    cmd_delete(root, picked, hide)?;
    let deleted = zorg::account::read_index(root)?.len() < accounts.len();
    if !deleted {
        println!();
        terminal::wait_for("Esc back")?;
    }
    Ok(deleted)
}

/// Settings → a network's server: the built-in list, or a URL that serves
/// `network`. Returns whether anything changed.
fn choose_server(root: &std::path::Path, network: WalletNetwork) -> Result<bool, String> {
    terminal::clear()?;
    let title = format!("{} server", network.display_name());
    println!("{}\n", console::style(title).bold());
    let choices = ["Built-in (with failover)", "Custom URL"].map(String::from);
    let Some(choice) = terminal::menu("Esc back", &choices, 0)? else {
        return Ok(false);
    };
    if choice == 0 {
        zorg::account::set_server(root, network, None)?;
        return Ok(true);
    }
    let url = terminal::ask("Server URL", false, |_| Ok(()))?;
    RUNTIME.block_on(zorg::sync_engine::connect_endpoint(network, Some(&url)))?;
    zorg::account::set_server(root, network, Some(&url))?;
    Ok(true)
}

/// Settings → Viewing key: `account`'s unified full viewing key and birthday,
/// with Copy, until Esc goes back.
fn show_viewing_key(account: &zorg::account::Account) -> Result<(), String> {
    let key = zorg::account::viewing_key(account)?;
    let birthday = zorg::account::birthday_height(account)?;
    let copy = ["Copy viewing key".to_string()];
    let mut status: Result<String, String> = Ok(String::new());
    loop {
        terminal::clear()?;
        println!("{}\n", console::style("Viewing key").bold());
        println!("{key}\n");
        println!("{}\n", terminal::row("birthday", 8, birthday));
        terminal::note("Anyone with this key can see every transaction of this account.");
        match &status {
            Ok(message) if message.is_empty() => {}
            Ok(message) => terminal::note(message),
            Err(e) => terminal::failure(e),
        }
        println!();
        status = match terminal::menu("Esc back", &copy, 0)? {
            None => return Ok(()),
            Some(_) => terminal::copy(&key).map(|()| "Copied.".into()),
        };
    }
}

/// The History screen: `account`'s transactions, pending first and then
/// newest first, with amounts as dots when `hide` is set, and Enter to show
/// one in full, until Esc goes back.
fn history(account: &zorg::account::Account, hide: bool) -> Result<(), String> {
    let (db, net, uuid) = (
        account.db_path.as_str(),
        account.network,
        account.uuid.as_str(),
    );
    let txs = sync::get_transaction_history(db, None, uuid)?;
    if txs.is_empty() {
        println!("No transactions yet.\n");
        return terminal::wait_for("Esc back");
    }
    let mut current = 0;
    loop {
        terminal::clear()?;
        let term = console::Term::stdout();
        let (height, width) = term.size();
        let page_size = usize::from(height).saturating_sub(6).clamp(1, 10);
        let width = usize::from(width).saturating_sub(2);
        let now = terminal::unix_now();
        let items: Vec<String> = txs
            .iter()
            .map(|tx| {
                let (arrow, sign, kind) = match tx.tx_kind.as_str() {
                    "received" | "receiving" => {
                        (console::style("↓").green().to_string(), "+", "Received")
                    }
                    "sent" => ("↑".to_string(), "−", "Sent"),
                    "shielded" => ("↔".to_string(), " ", "Shielding"),
                    _ => ("↔".to_string(), " ", "Unknown"),
                };
                let amount = if hide {
                    "••••".to_string()
                } else {
                    terminal::zec(tx.display_amount, 2)
                };
                let amount = format!("{sign}{amount} {}", net.unit());
                let when = match tx.mined_height {
                    0 if tx.expired_unmined => "expired".to_string(),
                    0 => "pending".to_string(),
                    height if tx.block_time == 0 => format!("block {height}"),
                    _ => terminal::ago(now.saturating_sub(tx.block_time)),
                };
                let row = format!("{arrow} {kind:<10} {amount:>18}   {when}");
                console::truncate_str(&row, width, "…").into_owned()
            })
            .collect();

        println!("{}", console::style("Transactions").bold());
        println!("{} transactions\n", txs.len());
        let hint = console::truncate_str(
            "↑↓ move · ←→ / h/l page · Enter details · Esc back",
            width.saturating_sub(18),
            "…",
        );
        let Some(choice) =
            dialoguer::Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
                .with_prompt(hint)
                .items(&items)
                .max_length(page_size)
                .default(current)
                .report(false)
                .interact_on_opt(&term)
                .map_err(|e| e.to_string())?
        else {
            return Ok(());
        };
        current = choice;
        terminal::clear()?;
        if let Err(e) = cmd_tx(account, &txs[current].txid.to_string(), hide) {
            terminal::failure(&e);
        }
        println!();
        terminal::wait_for("Esc back")?;
    }
}

/// Deletes `account` after confirming: its index entry, its database if one is
/// left, and its stored recovery phrase. The card shows the balance as dots when
/// `hide` is set.
fn cmd_delete(
    root: &std::path::Path,
    account: &zorg::account::Account,
    hide: bool,
) -> Result<(), String> {
    let interactive = terminal::interactive();
    let address = zorg::account::current_address(account).ok();
    if interactive {
        let unit = account.network.unit();
        let balance = address.as_ref().map(|_| {
            sync::get_wallet_balance(&account.db_path, account.network, &account.uuid).map_or(
                "unknown".to_string(),
                |b| {
                    let amount = if hide {
                        "••••".into()
                    } else {
                        terminal::zec(b.total, 2)
                    };
                    format!("{amount} {unit}")
                },
            )
        });
        let card = |full: bool| {
            let mut lines = vec![format!(
                "Delete account {}",
                console::style(&account.name).bold()
            )];
            if let (Some(address), Some(balance)) = (&address, &balance) {
                lines.push(terminal::row(
                    "address",
                    7,
                    terminal::address(address, full),
                ));
                lines.push(terminal::row("balance", 7, balance));
            }
            lines.push(String::new());
            lines.push(format!(
                "{} zorg can't show the recovery phrase again. Keep your written copy.",
                console::style("!").yellow().bold()
            ));
            lines
        };
        let width = address.as_ref().map_or(0, String::len);
        match terminal::confirm("delete", width, card) {
            Ok(()) => {}
            Err(e) if e == terminal::CANCELLED => {
                terminal::note("Cancelled. Nothing was deleted.");
                return Ok(());
            }
            Err(e) => return Err(e),
        }
    } else {
        match &address {
            Some(address) => println!("This deletes account {:?} ({address}).", account.name),
            None => println!("This deletes account {:?}.", account.name),
        }
        println!("Keep the written-down recovery phrase: zorg can't show it again.");
    }
    zorg::secret_store::authenticate(&account.db_path, account.network, &account.uuid)?;
    zorg::account::delete_account(root, account, || {
        zorg::secret_store::delete_mnemonic(&account.db_path, account.network, &account.uuid)
    })?;
    if interactive {
        terminal::success(&format!("Deleted account {}", account.name));
    } else {
        println!("Account {:?} deleted.", account.name);
    }
    Ok(())
}

/// The error for an account with no recovery phrase in the selected store.
fn missing_phrase_error(account: &zorg::account::Account, birthday: u32) -> String {
    let network = match account.network {
        WalletNetwork::Main => String::new(),
        net => format!(" --network {}", net.as_str()),
    };
    format!(
        "The recovery phrase for {name:?} isn't in this build's store on this Mac. \
         Restore it with your written-down phrase:\n  \
         zorg delete --account {name:?}\n  \
         zorg create {name:?} --import{network} --birthday {birthday}",
        name = account.name,
    )
}

/// The account's recovery phrase, or the commands that restore it when this Mac has none.
fn phrase(account: &zorg::account::Account) -> Result<zeroize::Zeroizing<String>, String> {
    match zorg::secret_store::mnemonic_if_present(&account.db_path, account.network, &account.uuid)?
    {
        Some(phrase) => Ok(phrase),
        None => Err(missing_phrase_error(
            account,
            zorg::account::birthday_height(account)?,
        )),
    }
}

fn show_recovery_phrase(account: &zorg::account::Account) -> Result<(), String> {
    let birthday = zorg::account::birthday_height(account)?;
    let mnemonic = phrase(account)?;
    let copy = ["Copy phrase".to_string()];
    let mut status: Result<String, String> = Ok(String::new());
    terminal::clear()?;
    loop {
        print!(
            "{}",
            *terminal::recovery_phrase_text(&mnemonic, u64::from(birthday))
        );
        match &status {
            Ok(message) => terminal::note(message),
            Err(error) => terminal::failure(error),
        }
        let choice = terminal::menu_with_keys("Esc back", &copy, 0, &[], &mut || {});
        terminal::clear()?;
        status = match choice? {
            None => return Ok(()),
            Some(_) => terminal::copy(&mnemonic).map(|()| "Copied.".into()),
        };
    }
}

fn cmd_recovery(account: &zorg::account::Account) -> Result<(), String> {
    let birthday = zorg::account::birthday_height(account)?;
    print!(
        "{}",
        *terminal::recovery_phrase_text(&phrase(account)?, u64::from(birthday))
    );
    Ok(())
}

fn cmd_balance(db: &str, net: WalletNetwork, uuid: &str) -> Result<(), String> {
    let bal = sync::get_wallet_balance(db, net, uuid)?;
    println!("Spendable:  {}", terminal::fmt_zec(bal.spendable, net));
    let pending = bal.change_pending_confirmation + bal.value_pending_spendability;
    println!("Pending:    {}", terminal::fmt_zec(pending, net));
    println!("Locked:     {}", terminal::fmt_zec(bal.locked, net));
    println!("Total:      {}", terminal::fmt_zec(bal.total, net));
    for (pool, value) in [
        ("Transparent:", bal.transparent),
        ("Sapling:", bal.sapling),
        ("Orchard:", bal.orchard),
        ("Ironwood:", bal.ironwood),
    ] {
        if value > 0 {
            println!("  {pool:<13} {}", terminal::fmt_zec(value, net));
        }
    }
    Ok(())
}

/// The pools a Zcash address can receive into.
fn address_receivers(address: &str) -> Result<Vec<zcash_protocol::PoolType>, String> {
    use zcash_protocol::PoolType;

    let address: zcash_keys::address::Address = address
        .parse::<zcash_address::ZcashAddress>()
        .map_err(|e| format!("Invalid: {e}"))?
        .convert()
        .map_err(|e| format!("Invalid: {e}"))?;
    Ok(
        [PoolType::ORCHARD, PoolType::SAPLING, PoolType::TRANSPARENT]
            .into_iter()
            .filter(|pool| address.can_receive_as(*pool))
            .collect(),
    )
}

fn cmd_send(
    account: &zorg::account::Account,
    server: Option<&str>,
    positional: &[String],
    memo: Option<&str>,
) -> Result<(), String> {
    let (db, net, uuid) = (
        account.db_path.as_str(),
        account.network,
        account.uuid.as_str(),
    );
    let mut client = RUNTIME.block_on(zorg::sync_engine::connect_endpoint(net, server))?;
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
    let to_address = to
        .parse::<zcash_address::ZcashAddress>()
        .map_err(|e| format!("Bad address: {e}"))?;
    let asked_memo;
    let memo = match memo {
        None if guided && to_address.can_receive_memo() => {
            asked_memo = terminal::ask("Memo (optional)", true, |_| Ok(()))?;
            (!asked_memo.is_empty()).then_some(asked_memo.as_str())
        }
        memo => memo,
    };
    // The card shows an estimate, so nothing is locked while it waits.
    let fee = if interactive {
        sync::estimate_fee(db, net, uuid, to, amount_zat, memo)?
    } else {
        0
    };
    let card = |full: bool| {
        let total = amount_zat + fee;
        let mut lines = vec![
            format!(
                "Send {} {unit}   {}",
                console::style(terminal::zec(amount_zat, 0)).bold(),
                terminal::privacy_tag(!to_address.is_transparent_only())
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

    let confirmed = if interactive {
        terminal::confirm("send", to.len(), card)
    } else {
        Ok(())
    };
    let result = confirmed.and_then(|()| {
        let proposal = sync::propose_send(db, net, uuid, to, amount_zat, memo)?;
        let proposed_fee = proposal.fee_zatoshi;
        RUNTIME.block_on(sync::execute_proposal_with_seed_loader(
            &mut client,
            proposal,
            |_, _| {
                // A failed seed load releases the proposal's inputs, so failing here does too.
                if interactive {
                    if proposed_fee != fee {
                        return Err(format!(
                            "the fee changed to {} {unit}; send again",
                            terminal::zec(proposed_fee, 0)
                        ));
                    }
                    progress.show("Waiting for Touch ID…");
                } else {
                    print!(
                        "{}",
                        terminal::send_summary(to, amount_zat, proposed_fee, memo, net)
                    );
                }
                let seed = keys::mnemonic_to_seed(&phrase(account)?)?;
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
        ))
    });
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

/// Moves `account`'s transparent funds into its shielded balance, after the
/// same authorization as a send.
fn cmd_shield(account: &zorg::account::Account, server: Option<&str>) -> Result<(), String> {
    let net = account.network;
    let mut client = RUNTIME.block_on(zorg::sync_engine::connect_endpoint(net, server))?;
    let interactive = terminal::interactive();
    let unit = net.unit();
    let proposal = sync::propose_shield(&account.db_path, net, &account.uuid)?;
    let fee = proposal.fee_zatoshi;
    let amount = proposal.transparent_input_zatoshi().saturating_sub(fee);
    let card = |_full: bool| {
        vec![
            format!(
                "Shield {} {unit}   {}",
                console::style(terminal::zec(amount, 0)).bold(),
                terminal::privacy_tag(true)
            ),
            terminal::row("from", 5, format!("{} · transparent", account.name)),
            terminal::row("fee", 5, format!("{} {unit}", terminal::zec(fee, 0))),
        ]
    };
    let progress = terminal::Progress::hidden();
    let broadcasting = std::cell::Cell::new(false);

    let result = RUNTIME.block_on(sync::execute_proposal_with_seed_loader(
        &mut client,
        proposal,
        |_, _| {
            // A failed seed load releases the proposal's inputs, so cancelling here does too.
            if interactive {
                terminal::confirm("shield", 0, card)?;
                progress.show("Waiting for Touch ID…");
            } else {
                print!(
                    "Shield {}\nFee: {}\n",
                    terminal::fmt_zec(amount, net),
                    terminal::fmt_zec(fee, net)
                );
            }
            let seed = keys::mnemonic_to_seed(&phrase(account)?)?;
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
            terminal::success(&format!("Shielded {} {unit}", terminal::zec(amount, 0)));
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
            terminal::note("Cancelled. Nothing was shielded.");
            Ok(())
        }
        Err(e) if interactive && e != terminal::INTERRUPTED => {
            if broadcasting.get() {
                terminal::failure(&format!(
                    "Broadcast failed: {e}. The transaction is saved and zorg retries it on \
                     the next sync — check zorg history before shielding again."
                ));
            } else {
                terminal::failure(&format!("Not shielded: {e}"));
            }
            Err(terminal::REPORTED.into())
        }
        Err(e) => Err(e),
    }
}

fn cmd_history(db: &str, net: WalletNetwork, limit: Option<u32>, uuid: &str) -> Result<(), String> {
    let txs = sync::get_transaction_history(db, limit, uuid)?;
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
            terminal::fmt_zec(tx.display_amount, net),
            height,
            tx.txid
        );
    }
    Ok(())
}

/// Prints the transaction `txid` in full: one section per history row it makes,
/// with amounts as dots when `hide` is set.
fn cmd_tx(account: &zorg::account::Account, txid: &str, hide: bool) -> Result<(), String> {
    let (db, net, uuid) = (
        account.db_path.as_str(),
        account.network,
        account.uuid.as_str(),
    );
    let txid = parse_txid(txid)?;
    let rows: Vec<_> = sync::get_transaction_history(db, None, uuid)?
        .into_iter()
        .filter(|tx| tx.txid == txid)
        .collect();
    if rows.is_empty() {
        return Err(format!(
            "No transaction {txid} in account {:?}",
            account.name
        ));
    }
    let unit = net.unit();
    let zec = |zatoshis| {
        if hide {
            "••••".into()
        } else {
            terminal::zec(zatoshis, 0)
        }
    };
    for tx in rows {
        let detail = sync::get_transaction_detail(db, net, uuid, &txid, &tx.tx_kind)?;
        let status = if tx.mined_height > 0 {
            format!("mined at block {}", tx.mined_height)
        } else if tx.expired_unmined {
            "expired, never mined".to_string()
        } else {
            "pending".to_string()
        };
        let mut kind = tx.tx_kind.clone();
        if let Some(first) = kind.get_mut(..1) {
            first.make_ascii_uppercase();
        }
        println!("{kind} {} {unit}   {status}", zec(tx.display_amount));
        if let Some(from) = &detail.source_address {
            println!("{}", terminal::row("from", 4, from));
        }
        for output in &detail.outputs {
            let to = output.address.as_deref().unwrap_or("(unknown)");
            if detail.outputs.len() > 1 {
                let amount = zec(output.amount_zatoshi);
                println!(
                    "{}",
                    terminal::row("to", 4, format!("{to}  {amount} {unit}"))
                );
                if let Some(memo) = &output.memo {
                    println!("{}", terminal::row("memo", 4, format!("{memo:?}")));
                }
            } else {
                println!("{}", terminal::row("to", 4, to));
            }
        }
        println!("{}", terminal::row("pool", 4, &tx.display_pool));
        if tx.tx_kind != "received" && tx.tx_kind != "receiving" {
            let fee = zec(tx.fee);
            println!("{}", terminal::row("fee", 4, format!("{fee} {unit}")));
        }
        if let [output] = detail.outputs.as_slice() {
            if let Some(memo) = &output.memo {
                println!("{}", terminal::row("memo", 4, format!("{memo:?}")));
            }
        }
    }
    println!("{}", terminal::row("txid", 4, txid));
    Ok(())
}

/// The txid in `text`, written the way block explorers and `zorg history` show it.
fn parse_txid(text: &str) -> Result<zcash_primitives::transaction::TxId, String> {
    let mut bytes: [u8; 32] = hex::decode(text.trim())
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or("Not a valid txid; expected 64 hexadecimal characters")?;
    bytes.reverse();
    Ok(zcash_primitives::transaction::TxId::from_bytes(bytes))
}

// ── helpers ──

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_account_commands_use_the_owning_database_on_regtest() {
        let root = tempfile::tempdir().unwrap();
        let index_root = root.path().to_path_buf();
        zorg::account::ensure_private_wallet_dir(root.path()).unwrap();
        let mut accounts = Vec::new();
        for name in ["Daily", "Savings", "Travel"] {
            let seed = zorg::keys::mnemonic_to_seed(&zorg::keys::generate_mnemonic()).unwrap();
            let db = root.path().join(format!("{name}.db"));
            let created = zorg::account::init_db_and_create_account(
                db.to_str().unwrap(),
                WalletNetwork::Regtest,
                &seed,
                None,
                name,
            )
            .unwrap();
            let account = zorg::account::Account {
                uuid: created.uuid,
                name: name.into(),
                network: WalletNetwork::Regtest,
                db_path: db.to_str().unwrap().into(),
            };
            zorg::account::index_new_account(root.path(), &account, &seed).unwrap();
            accounts.push(account);
        }
        let command = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|arg| arg.to_string()).collect();
            parse_args(&args).and_then(|args| super::run_in(index_root.clone(), args))
        };
        let names = || -> Vec<String> {
            zorg::account::read_index(root.path())
                .unwrap()
                .into_iter()
                .map(|account| account.name)
                .collect()
        };
        command(&[]).unwrap();
        assert!(command(&["balance"]).unwrap_err().contains("--account"));
        command(&["address", "--account", "Savings"]).unwrap();
        command(&["balance", "--account", "Travel"]).unwrap();
        command(&["history", "--account", "Daily"]).unwrap();

        zorg::account::delete_account(root.path(), &accounts[0], || Ok(())).unwrap();
        assert!(!std::path::Path::new(&accounts[0].db_path).exists());
        assert_eq!(names(), ["Savings", "Travel"]);

        let offline = root.path().join("offline.db");
        std::fs::rename(&accounts[2].db_path, &offline).unwrap();
        assert!(command(&["balance"]).unwrap_err().contains("unavailable"));
        assert!(command(&["balance", "--account", "Travel"])
            .unwrap_err()
            .contains("database missing"));
        assert_eq!(command(&["status"]).unwrap_err(), "1 of 2 databases failed");
        command(&["status", "--account", "Savings"]).unwrap();
        command(&["balance", "--account", "Savings"]).unwrap();
        std::fs::rename(&offline, &accounts[2].db_path).unwrap();

        let mut travel = accounts[2].clone();
        travel.uuid = accounts[1].uuid.clone();
        assert!(
            zorg::account::delete_account(root.path(), &travel, || Ok(()))
                .unwrap_err()
                .contains("not deleting")
        );
        assert!(std::path::Path::new(&accounts[2].db_path).exists());

        std::fs::rename(&accounts[2].db_path, &offline).unwrap();
        zorg::account::delete_account(root.path(), &accounts[2], || Ok(())).unwrap();
        assert!(offline.exists());
        assert_eq!(names(), ["Savings"]);
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
        assert_eq!(parse(&[]).unwrap().command, None);
        assert!(parse(&["--network", "test"]).is_err());
        assert!(parse(&["balance", "--network", "test"]).is_err());
        assert_eq!(
            parse(&["create", "Savings", "--import", "--network", "test"])
                .unwrap()
                .options["--network"],
            "test"
        );
        assert_eq!(
            parse(&["accounts"]).unwrap().command.as_deref(),
            Some("accounts")
        );
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
        assert!(parse(&["address", "--new"])
            .unwrap()
            .options
            .contains_key("--new"));
        let args = parse(&["address", "--new", "--account", "Savings"]).unwrap();
        assert_eq!(args.options["--new"], "");
        assert_eq!(args.options["--account"], "Savings");
        assert!(parse(&["address", "--new", "--new"]).is_err());
        assert!(parse(&["balance", "--new"]).is_err());
    }

    #[test]
    fn first_run_lists_no_accounts_without_creating_a_database() {
        let root = tempfile::tempdir().unwrap();
        super::run_in(root.path().to_path_buf(), parse_args(&[]).unwrap()).unwrap();
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn missing_phrase_error_shows_the_restore_commands() {
        let account = zorg::account::Account {
            uuid: String::new(),
            name: "My Savings".into(),
            network: WalletNetwork::Test,
            db_path: String::new(),
        };
        let error = missing_phrase_error(&account, 2_345_600);
        assert!(
            error.contains("zorg delete --account \"My Savings\""),
            "{error}"
        );
        assert!(
            error.contains("zorg create \"My Savings\" --import --network test --birthday 2345600"),
            "{error}"
        );
    }

    #[test]
    fn address_receivers_lists_the_pools_an_address_can_receive_into() {
        use zcash_address::{ToAddress, ZcashAddress};
        use zcash_keys::{address::UnifiedAddress, keys::UnifiedSpendingKey};
        use zcash_protocol::{consensus::NetworkType, PoolType};

        let net = WalletNetwork::Regtest;
        let (shielded, _) = UnifiedSpendingKey::from_seed(&net, &[7; 32], 0u32.try_into().unwrap())
            .unwrap()
            .to_unified_full_viewing_key()
            .default_address(keys::shielded_address_request())
            .unwrap();
        let transparent = UnifiedAddress::from_receivers(
            None,
            None,
            Some(transparent::address::TransparentAddress::PublicKeyHash(
                [1; 20],
            )),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            address_receivers(&shielded.encode(&net)).unwrap(),
            [PoolType::ORCHARD, PoolType::SAPLING]
        );
        assert_eq!(
            address_receivers(&transparent.encode(&net)).unwrap(),
            [PoolType::TRANSPARENT]
        );
        assert_eq!(
            address_receivers("t1Hsc1LR8yKnbbe3twRp88p6vFfC5t7DLbs").unwrap(),
            [PoolType::TRANSPARENT]
        );
        let sprout = ZcashAddress::from_sprout(NetworkType::Main, [0; 64]).encode();
        assert!(address_receivers(&sprout).is_err());
        assert!(address_receivers("not an address").is_err());
    }

    #[test]
    fn txids_are_read_the_way_they_are_shown() {
        let shown = "02b17177492419c635fdcdc75cd5e12531d50bd6ffff59ffdfce5b36f5bf9220";
        assert_eq!(parse_txid(shown).unwrap().to_string(), shown);
        assert!(parse_txid("02b171").is_err());
        assert!(parse_txid(&"zz".repeat(32)).is_err());
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
}
