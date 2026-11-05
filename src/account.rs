use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

use secrecy::{ExposeSecret, SecretVec};
use transparent::keys::IncomingViewingKey;

use zcash_client_backend::data_api::{
    chain::ChainState, Account as _, AccountBirthday, WalletRead, WalletWrite,
};
use zcash_client_sqlite::{wallet::init::init_wallet_db, AccountUuid};
use zcash_primitives::block::BlockHash;
use zcash_protocol::consensus::{NetworkUpgrade, Parameters};

use crate::{
    db::{
        open_wallet_db_for_read_with_timeout, open_wallet_db_with_timeout,
        with_wallet_db_write_lock, WalletDatabase, ACCOUNT_MUTATION_DB_BUSY_TIMEOUT,
        READ_DB_BUSY_TIMEOUT, WALLET_DB_BUSY_TIMEOUT,
    },
    network::WalletNetwork,
};

use crate::keys::*;

// ======================== Public API Structs ========================

/// A newly created account.
pub struct CreatedAccount {
    pub uuid: String,
    pub unified_address: String,
}

/// An account as `zorg_index.db` records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub uuid: String,
    pub name: String,
    pub network: WalletNetwork,
    pub db_path: String,
}

fn open_wallet_db_for_init(
    db_path: &str,
    network: WalletNetwork,
) -> Result<WalletDatabase, String> {
    open_wallet_db_with_timeout(db_path, network, WALLET_DB_BUSY_TIMEOUT)
}

fn open_wallet_db_for_read(
    db_path: &str,
    network: WalletNetwork,
) -> Result<WalletDatabase, String> {
    open_wallet_db_for_read_with_timeout(db_path, network, READ_DB_BUSY_TIMEOUT)
}

/// Ensure the wallet DB schema is initialized and migrated once per process.
///
/// This intentionally runs without a seed. Most migrations, including the
/// current zcash_client_sqlite 0.20.x migrations, do not require one; passing an
/// arbitrary seed would be incorrect for Imported-only and multi-seed wallets.
pub fn ensure_db_migrated_once(db_path: &str, network: WalletNetwork) -> Result<(), String> {
    static MIGRATED_DBS: OnceLock<Mutex<HashSet<(String, WalletNetwork)>>> = OnceLock::new();

    let key = (db_path.to_string(), network);
    let migrated_dbs = MIGRATED_DBS.get_or_init(|| Mutex::new(HashSet::new()));
    let mut migrated = match migrated_dbs.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            log::error!("wallet DB migration gate poisoned; continuing");
            poisoned.into_inner()
        }
    };

    if migrated.contains(&key) {
        return Ok(());
    }

    log::info!("wallet DB migration gate: ensuring schema for {db_path}");
    with_wallet_db_write_lock("keys.ensure_db_initialized", || {
        let mut db = open_wallet_db_for_init(db_path, network)?;
        init_wallet_db(&mut db, None).map_err(|e| format!("Failed to init wallet DB: {e}"))?;
        Ok::<_, String>(())
    })?;
    migrated.insert(key);
    Ok(())
}

/// Init DB + create the bootstrap software account as Derived.
/// This pins the DB seed fingerprint for seed-aware initialization.
/// The account receives the supplied name and the next derivation index.
/// A `birthday` of `None` starts the account at Sapling activation.
pub fn init_db_and_create_account(
    db_path: &str,
    network: WalletNetwork,
    seed: &SecretVec<u8>,
    birthday: Option<AccountBirthday>,
    name: &str,
) -> Result<CreatedAccount, String> {
    // The seed is needed so that seed-requiring migrations can run in the future.
    with_wallet_db_write_lock("keys.ensure_db_initialized_with_seed", || {
        let mut db = open_wallet_db_for_init(db_path, network)?;
        init_wallet_db(&mut db, Some(SecretVec::new(seed.expose_secret().to_vec())))
            .map_err(|e| format!("Failed to init wallet DB: {e}"))?;
        Ok::<_, String>(())
    })?;

    let birthday = birthday.unwrap_or_else(|| {
        let sapling = network
            .activation_height(NetworkUpgrade::Sapling)
            .expect("Sapling activation height must be known");
        AccountBirthday::from_parts(ChainState::empty(sapling - 1, BlockHash([0u8; 32])), None)
    });

    let (account_id, usk) = with_wallet_db_write_lock("keys.create_account", || {
        let mut db =
            open_wallet_db_with_timeout(db_path, network, ACCOUNT_MUTATION_DB_BUSY_TIMEOUT)?;

        let (account_id, usk) = db
            .create_account(name, seed, &birthday, None)
            .map_err(|e| format!("Failed to create account: {e}"))?;
        Ok::<_, String>((account_id, usk))
    })?;

    let ufvk = usk.to_unified_full_viewing_key();
    let (ua, _di) = ufvk
        .default_address(shielded_address_request())
        .map_err(|e| format!("Failed to derive address: {e}"))?;

    Ok(CreatedAccount {
        uuid: account_id.expose_uuid().to_string(),
        unified_address: ua.encode(&network),
    })
}

/// The UUIDs of the accounts in the wallet database.
pub fn list_accounts(db_path: &str, network: WalletNetwork) -> Result<Vec<String>, String> {
    let db = open_wallet_db_for_read(db_path, network)?;
    let account_ids = db
        .get_account_ids()
        .map_err(|e| format!("Failed to list accounts: {e}"))?;
    Ok(account_ids
        .into_iter()
        .map(|id| id.expose_uuid().to_string())
        .collect())
}

/// Fails when `account`'s database file is missing.
pub fn ensure_database_exists(account: &Account) -> Result<(), String> {
    if Path::new(&account.db_path).exists() {
        Ok(())
    } else {
        Err(format!(
            "{:?}: database missing ({})",
            account.name, account.db_path
        ))
    }
}

/// `account`'s current receiving address, read from its database.
pub fn current_address(account: &Account) -> Result<String, String> {
    ensure_database_exists(account)?;
    let db = open_wallet_db_for_read(&account.db_path, account.network)?;
    let id = parse_account_uuid(&account.uuid)?;
    let ufvk = db
        .get_account(id)
        .map_err(|e| format!("Failed to get account: {e}"))?
        .and_then(|a| a.ufvk().cloned())
        .ok_or_else(|| format!("Account not found: {}", account.uuid))?;
    let (default, _) = ufvk
        .default_address(shielded_address_request())
        .map_err(|e| format!("Failed to derive shielded address: {e}"))?;
    let last = db
        .get_last_generated_address_matching(id, shielded_address_request())
        .map_err(|e| format!("Failed to get last generated shielded address: {e}"))?;
    Ok(last.unwrap_or(default).encode(&account.network))
}

/// `account`'s transparent receiving address: external index 0 of its viewing key.
pub fn transparent_address(account: &Account) -> Result<String, String> {
    ensure_database_exists(account)?;
    let db = open_wallet_db_for_read(&account.db_path, account.network)?;
    let id = parse_account_uuid(&account.uuid)?;
    let ufvk = db
        .get_account(id)
        .map_err(|e| format!("Failed to get account: {e}"))?
        .and_then(|a| a.ufvk().cloned())
        .ok_or_else(|| format!("Account not found: {}", account.uuid))?;
    let (address, _) = ufvk
        .p2pkh()
        .ok_or("Account has no transparent key")?
        .derive_external_ivk()
        .map_err(|e| format!("Failed to derive transparent key: {e}"))?
        .default_address();
    Ok(zcash_keys::encoding::encode_transparent_address_p(
        &account.network,
        &address,
    ))
}

/// The height `account` scans from, read from its database.
pub fn birthday_height(account: &Account) -> Result<u32, String> {
    ensure_database_exists(account)?;
    let db = open_wallet_db_for_read(&account.db_path, account.network)?;
    let id = parse_account_uuid(&account.uuid)?;
    db.get_account(id)
        .map_err(|e| format!("Failed to get account: {e}"))?
        .map(|a| u32::from(a.birthday_height()))
        .ok_or_else(|| format!("Account not found: {}", account.uuid))
}

/// Parse an account UUID string into the backend's AccountUuid.
pub fn parse_account_uuid(s: &str) -> Result<AccountUuid, String> {
    let uuid = uuid::Uuid::parse_str(s).map_err(|e| format!("Invalid account UUID: {e}"))?;
    Ok(AccountUuid::from_uuid(uuid))
}

/// The selected build's wallet directory: `~/.zorg` for standalone builds,
/// or `~/Library/Application Support/zorg` for packaged builds.
pub fn wallet_dir() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    let directory = if cfg!(feature = "packaged") {
        "Library/Application Support/zorg"
    } else {
        ".zorg"
    };
    Ok(PathBuf::from(home).join(directory))
}

/// Opens `zorg_index.db` and holds its write lock until `COMMIT`.
pub fn open_index_for_write(root: &Path) -> Result<rusqlite::Connection, String> {
    ensure_private_wallet_dir(root)?;
    let path = root.join("zorg_index.db");
    let conn = rusqlite::Connection::open(&path)
        .map_err(|e| format!("Could not open database index: {e}"))?;
    secure_wallet_db_file(&path)?;
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         BEGIN IMMEDIATE;
         CREATE TABLE IF NOT EXISTS accounts (
             uuid TEXT PRIMARY KEY NOT NULL,
             name TEXT NOT NULL UNIQUE,
             network TEXT NOT NULL,
             db_path TEXT NOT NULL UNIQUE
         );
         CREATE TABLE IF NOT EXISTS settings (
             id INTEGER PRIMARY KEY CHECK (id = 1),
             user_name TEXT NOT NULL,
             default_account TEXT REFERENCES accounts(uuid) ON DELETE SET NULL,
             hide_balances INTEGER NOT NULL DEFAULT 0,
             server_test TEXT,
             server_main TEXT
         );
         INSERT OR IGNORE INTO settings (id, user_name, server_test)
             VALUES (1, 'ZORG', 'https://testnet.zec.rocks:443');",
    )
    .map_err(|e| format!("Could not lock database index: {e}"))?;
    Ok(conn)
}

/// The accounts in an open `zorg_index.db`, in the order they were added.
fn indexed_accounts(conn: &rusqlite::Connection) -> Result<Vec<Account>, String> {
    let error = |e: rusqlite::Error| format!("Could not read database index: {e}");
    let mut stmt = conn
        .prepare("SELECT uuid, name, network, db_path FROM accounts ORDER BY rowid")
        .map_err(error)?;
    let rows = stmt
        .query_map([], |row| {
            Ok([row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?])
        })
        .and_then(|rows| rows.collect::<Result<Vec<[String; 4]>, _>>())
        .map_err(error)?;
    rows.into_iter()
        .map(|[uuid, name, network, db_path]| {
            Ok(Account {
                uuid,
                name,
                network: network.parse()?,
                db_path,
            })
        })
        .collect()
}

/// The accounts in `zorg_index.db`, or none when it does not exist.
pub fn read_index(root: &Path) -> Result<Vec<Account>, String> {
    match open_index_for_read(root, "accounts")? {
        Some(conn) => indexed_accounts(&conn),
        None => Ok(Vec::new()),
    }
}

/// Opens `zorg_index.db` to read `table`, or none when the index has no such
/// table yet: no file, or a first write that never finished. It opens
/// read-write, without creating the file, so SQLite can roll back that write.
fn open_index_for_read(root: &Path, table: &str) -> Result<Option<rusqlite::Connection>, String> {
    let path = root.join("zorg_index.db");
    if !path.exists() {
        return Ok(None);
    }
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(|e| format!("Could not open database index: {e}"))?;
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [table],
            |row| row.get(0),
        )
        .map_err(|e| format!("Could not read database index: {e}"))?;
    Ok(exists.then_some(conn))
}

/// What the settings row of `zorg_index.db` holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settings {
    /// The account the app opens, or none for the first account.
    pub default_account: Option<String>,
    /// Whether the app shows balances as dots.
    pub hide_balances: bool,
    /// The testnet server, or none for the built-in list.
    pub server_test: Option<String>,
    /// The mainnet server, or none for the built-in list.
    pub server_main: Option<String>,
}

impl Settings {
    /// The saved server for `network`, or none for the built-in list.
    pub fn server(&self, network: WalletNetwork) -> Option<String> {
        match network {
            WalletNetwork::Main => self.server_main.clone(),
            WalletNetwork::Test => self.server_test.clone(),
            WalletNetwork::Regtest => None,
        }
    }
}

/// The settings in `zorg_index.db`, or the defaults when none are saved yet.
pub fn read_settings(root: &Path) -> Result<Settings, String> {
    let Some(conn) = open_index_for_read(root, "settings")? else {
        return Ok(Settings::default());
    };
    conn.query_row(
        "SELECT default_account, hide_balances, server_test, server_main FROM settings",
        [],
        |row| {
            Ok(Settings {
                default_account: row.get(0)?,
                hide_balances: row.get(1)?,
                server_test: row.get(2)?,
                server_main: row.get(3)?,
            })
        },
    )
    .map_err(|e| format!("Could not read settings: {e}"))
}

/// Runs `sql` on the settings row of `zorg_index.db`.
fn update_settings(root: &Path, sql: &str, params: impl rusqlite::Params) -> Result<(), String> {
    let index = open_index_for_write(root)?;
    index
        .execute(sql, params)
        .and_then(|_| index.execute_batch("COMMIT"))
        .map_err(|e| format!("Could not save settings: {e}"))
}

/// Makes `uuid` the account the app opens.
pub fn set_default_account(root: &Path, uuid: &str) -> Result<(), String> {
    update_settings(root, "UPDATE settings SET default_account = ?1", [uuid])
}

/// Sets whether the app shows balances as dots.
pub fn set_hide_balances(root: &Path, hide: bool) -> Result<(), String> {
    update_settings(root, "UPDATE settings SET hide_balances = ?1", [hide])
}

/// Saves `url` as `network`'s server, or none for the built-in list.
pub fn set_server(root: &Path, network: WalletNetwork, url: Option<&str>) -> Result<(), String> {
    let sql = match network {
        WalletNetwork::Main => "UPDATE settings SET server_main = ?1",
        WalletNetwork::Test => "UPDATE settings SET server_test = ?1",
        WalletNetwork::Regtest => return Err("Regtest has no saved server; use --lwd".into()),
    };
    update_settings(root, sql, [url])
}

/// `account`'s unified full viewing key, read from its database.
pub fn viewing_key(account: &Account) -> Result<String, String> {
    ensure_database_exists(account)?;
    let db = open_wallet_db_for_read(&account.db_path, account.network)?;
    let id = parse_account_uuid(&account.uuid)?;
    let key = db
        .get_account(id)
        .map_err(|e| format!("Failed to get account: {e}"))?
        .and_then(|a| a.ufvk().cloned())
        .ok_or_else(|| format!("Account not found: {}", account.uuid))?;
    key.encode(&account.network)
        .map_err(|e| format!("Could not encode viewing key: {e}"))
}

/// Adds `account` to `zorg_index.db` and makes it the default account, after
/// checking that its name and seed are still unused.
pub fn index_new_account(
    root: &Path,
    account: &Account,
    seed: &SecretVec<u8>,
) -> Result<(), String> {
    let index = open_index_for_write(root)?;
    let accounts = indexed_accounts(&index)?;
    ensure_unique_name(&accounts, &account.name)?;
    ensure_new_seed(&accounts, seed, account.network)?;
    let db_path = account.db_path.as_str();
    index
        .execute(
            "INSERT INTO accounts (uuid, name, network, db_path) VALUES (?1, ?2, ?3, ?4)",
            [
                account.uuid.as_str(),
                account.name.as_str(),
                account.network.as_str(),
                db_path,
            ],
        )
        .map_err(|e| format!("Could not update database index: {e}"))?;
    index
        .execute(
            "UPDATE settings SET default_account = ?1",
            [account.uuid.as_str()],
        )
        .and_then(|_| index.execute_batch("COMMIT"))
        .map_err(|e| format!("Could not update database index: {e}"))
}

/// Deletes `account` from `zorg_index.db` along with its database files, if
/// any are left. Fails without deleting anything unless the database is in
/// `root` and, when it exists, holds exactly this account. Secret cleanup runs
/// after these checks and before database removal; a cleanup error keeps the index entry.
pub fn delete_account(
    root: &Path,
    account: &Account,
    delete_secret: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let index = open_index_for_write(root)?;
    let db_path = account.db_path.as_str();
    let in_root = Path::new(db_path)
        .parent()
        .and_then(|dir| dir.canonicalize().ok())
        .zip(root.canonicalize().ok())
        .is_some_and(|(dir, root)| dir == root);
    if !in_root {
        return Err(format!(
            "{db_path} is outside zorg's data directory; not deleting it"
        ));
    }
    if Path::new(db_path).exists() {
        match list_accounts(db_path, account.network)?.as_slice() {
            [only] if *only == account.uuid => {}
            _ => {
                return Err(format!(
                    "{db_path} does not hold exactly account {:?}; not deleting it",
                    account.name
                ))
            }
        }
    }
    index
        .execute("DELETE FROM accounts WHERE uuid = ?1", [&account.uuid])
        .map_err(|e| format!("Could not update database index: {e}"))?;
    // Dropping `index` without COMMIT keeps the row when cleanup fails.
    delete_secret()?;
    remove_wallet_files(db_path)?;
    index
        .execute_batch("COMMIT")
        .map_err(|e| format!("Could not update database index: {e}"))
}

/// Removes a wallet database's SQLite side files and then the database,
/// treating missing files as removed. Stops at the first other error.
pub fn remove_wallet_files(db: &str) -> Result<(), String> {
    for path in [format!("{db}-shm"), format!("{db}-wal"), db.to_string()] {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("Could not delete {path}: {e}")),
        }
    }
    Ok(())
}

/// Fails when one of `accounts` on `network` already holds accounts from
/// `seed`. Accounts whose database is missing are skipped.
pub fn ensure_new_seed(
    accounts: &[Account],
    seed: &secrecy::SecretVec<u8>,
    network: WalletNetwork,
) -> Result<(), String> {
    for account in accounts {
        if account.network != network || !Path::new(&account.db_path).exists() {
            continue;
        }
        let db = open_wallet_db_for_read(&account.db_path, network)?;
        let relevance = db
            .seed_relevance_to_derived_accounts(seed)
            .map_err(|e| format!("Failed to check recovery phrase: {e}"))?;
        if matches!(
            relevance,
            zcash_client_backend::data_api::SeedRelevance::Relevant { .. }
        ) {
            return Err(
                "This recovery phrase is already known on this network; run `zorg accounts`".into(),
            );
        }
    }
    Ok(())
}

/// The account named `name`.
pub fn select<'a>(accounts: &'a [Account], name: Option<&str>) -> Result<&'a Account, String> {
    match name {
        Some(name) => {
            let mut matches = accounts.iter().filter(|a| a.name == name);
            let account = matches
                .next()
                .ok_or_else(|| format!("No account named {name:?}; run `zorg accounts`"))?;
            if matches.next().is_some() {
                return Err(format!(
                    "Multiple accounts named {name:?}; give them unique names before selecting one"
                ));
            }
            Ok(account)
        }
        None if accounts.len() == 1 => Ok(&accounts[0]),
        None if accounts.is_empty() => {
            Err("No accounts found. Run `zorg create` or `zorg import`.".into())
        }
        None => Err("Choose an account with --account <name>; run `zorg accounts`".into()),
    }
}

/// Fails when `name` is invalid or already used by one of `accounts`.
pub fn ensure_unique_name(accounts: &[Account], name: &str) -> Result<(), String> {
    if name.is_empty() || name.trim() != name || name.chars().any(char::is_control) {
        return Err(
            "Account name must be nonempty, with no surrounding whitespace or control characters"
                .into(),
        );
    }
    if accounts.iter().any(|a| a.name == name) {
        Err(format!(
            "Account name {name:?} already exists; choose another name"
        ))
    } else {
        Ok(())
    }
}

/// Creates `path` if needed and sets its permissions to 0700.
pub fn ensure_private_wallet_dir(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .map_err(|e| format!("Failed to create wallet directory: {e}"))?;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("Failed to secure wallet directory: {e}"))
}

/// Sets an existing wallet file's permissions to 0600.
pub fn secure_wallet_db_file(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    if path.exists() {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("Failed to secure wallet database: {e}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_wallet_and_get_address() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("wallet.db");
        let db_path_str = db_path.to_str().unwrap();

        let phrase = generate_mnemonic();
        let seed = mnemonic_to_seed(&phrase).unwrap();

        let created =
            init_db_and_create_account(db_path_str, WalletNetwork::Main, &seed, None, "Account 1")
                .unwrap();
        let address = created.unified_address;

        // Verify we can read the address back
        let account = Account {
            uuid: created.uuid,
            name: "Account 1".into(),
            network: WalletNetwork::Main,
            db_path: db_path_str.into(),
        };
        assert_eq!(address, current_address(&account).unwrap());
    }

    #[test]
    fn test_get_address_returns_last_generated_receive_address() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("wallet.db");
        let db_path_str = db_path.to_str().unwrap();

        let phrase = generate_mnemonic();
        let seed = mnemonic_to_seed(&phrase).unwrap();

        let created =
            init_db_and_create_account(db_path_str, WalletNetwork::Main, &seed, None, "Account 1")
                .unwrap();
        let uuid = created.uuid.clone();
        let default_address = created.unified_address;
        let account = Account {
            uuid: created.uuid,
            name: "Account 1".into(),
            network: WalletNetwork::Main,
            db_path: db_path_str.into(),
        };

        crate::sync::update_chain_tip(db_path_str, WalletNetwork::Main, 2_500_000).unwrap();
        let renewed_address = crate::sync::get_next_available_address(
            db_path_str,
            WalletNetwork::Main,
            &uuid,
            crate::sync::AddressRequestKind::Shielded,
        )
        .unwrap();

        assert_ne!(default_address, renewed_address);
        assert_eq!(renewed_address, current_address(&account).unwrap());
    }

    #[test]
    fn delete_keeps_an_account_whose_database_cannot_be_deleted() {
        let root = tempfile::tempdir().unwrap();
        ensure_private_wallet_dir(root.path()).unwrap();
        let db = root.path().join("wallet.db");
        let db_str = db.to_str().unwrap();
        let seed = mnemonic_to_seed(&generate_mnemonic()).unwrap();
        let created =
            init_db_and_create_account(db_str, WalletNetwork::Regtest, &seed, None, "A").unwrap();
        let account = Account {
            uuid: created.uuid,
            name: "A".into(),
            network: WalletNetwork::Regtest,
            db_path: db_str.into(),
        };
        index_new_account(root.path(), &account, &seed).unwrap();
        set_default_account(root.path(), &account.uuid).unwrap();
        let chflags = |flag: &str| {
            assert!(std::process::Command::new("chflags")
                .args([flag, db_str])
                .status()
                .unwrap()
                .success());
        };

        chflags("uchg");
        let error = delete_account(root.path(), &account, || Ok(()));
        chflags("nouchg");
        assert!(error.unwrap_err().contains("Could not delete"));
        assert!(db.exists());
        assert_eq!(
            read_index(root.path()).unwrap(),
            std::slice::from_ref(&account)
        );

        delete_account(root.path(), &account, || Ok(())).unwrap();
        assert!(!db.exists());
        assert!(read_index(root.path()).unwrap().is_empty());
        assert_eq!(read_settings(root.path()).unwrap().default_account, None);
    }
}
