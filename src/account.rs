use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

use rusqlite::OptionalExtension;
use secrecy::{ExposeSecret, SecretVec};

use zcash_client_backend::data_api::{
    chain::ChainState, Account as _, AccountBirthday, WalletRead, WalletWrite,
};
use zcash_client_sqlite::{wallet::init::init_wallet_db, AccountUuid};
use zcash_keys::keys::UnifiedFullViewingKey;
use zcash_primitives::block::BlockHash;
use zcash_protocol::consensus::{BlockHeight, NetworkUpgrade, Parameters};

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

#[derive(Debug)]
pub struct AccountInfo {
    pub uuid: String,
    pub name: String,
    pub unified_address: String,
    pub account_index: u32,
    pub birthday_height: u32, // scan-from height
    pub db_path: String,
    pub network: WalletNetwork,
}

fn open_wallet_db_for_init(
    db_path: &str,
    network: WalletNetwork,
) -> Result<WalletDatabase, String> {
    open_wallet_db_with_timeout(db_path, network, WALLET_DB_BUSY_TIMEOUT)
}

fn open_wallet_db_for_mutation(
    db_path: &str,
    network: WalletNetwork,
) -> Result<WalletDatabase, String> {
    open_wallet_db_with_timeout(db_path, network, ACCOUNT_MUTATION_DB_BUSY_TIMEOUT)
}

fn open_wallet_db_for_read(
    db_path: &str,
    network: WalletNetwork,
) -> Result<WalletDatabase, String> {
    open_wallet_db_for_read_with_timeout(db_path, network, READ_DB_BUSY_TIMEOUT)
}

/// Initialize the wallet DB schema without binding a seed.
pub fn ensure_db_initialized(db_path: &str, network: WalletNetwork) -> Result<(), String> {
    with_wallet_db_write_lock("keys.ensure_db_initialized", || {
        let mut db = open_wallet_db_for_init(db_path, network)?;
        init_wallet_db(&mut db, None).map_err(|e| format!("Failed to init wallet DB: {e}"))?;
        Ok(())
    })
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
    ensure_db_initialized(db_path, network)?;
    migrated.insert(key);
    Ok(())
}

/// Initialize DB with seed for the first account (creates a Derived account).
/// The seed is needed so that seed-requiring migrations can run in the future.
fn ensure_db_initialized_with_seed(
    db_path: &str,
    network: WalletNetwork,
    seed: &SecretVec<u8>,
) -> Result<(), String> {
    with_wallet_db_write_lock("keys.ensure_db_initialized_with_seed", || {
        let mut db = open_wallet_db_for_init(db_path, network)?;
        init_wallet_db(&mut db, Some(SecretVec::new(seed.expose_secret().to_vec())))
            .map_err(|e| format!("Failed to init wallet DB: {e}"))?;
        Ok(())
    })
}

fn make_birthday(network: WalletNetwork, birthday_height: Option<u64>) -> AccountBirthday {
    match birthday_height {
        Some(h) => {
            let height = BlockHeight::from_u32(h as u32);
            let chain_state = ChainState::empty(height - 1, BlockHash([0u8; 32]));
            AccountBirthday::from_parts(chain_state, None)
        }
        None => {
            let sapling_height = network
                .activation_height(NetworkUpgrade::Sapling)
                .expect("Sapling activation height must be known");
            let chain_state = ChainState::empty(sapling_height - 1, BlockHash([0u8; 32]));
            AccountBirthday::from_parts(chain_state, None)
        }
    }
}

/// Init DB + create the bootstrap software account as Derived.
/// This pins the DB seed fingerprint for seed-aware initialization, but the
/// account may later be deleted like any other non-final account.
/// The account receives the supplied name and the next derivation index.
pub fn init_db_and_create_account(
    db_path: &str,
    network: WalletNetwork,
    seed: &SecretVec<u8>,
    birthday_height: Option<u64>,
    name: &str,
) -> Result<CreatedAccount, String> {
    ensure_db_initialized_with_seed(db_path, network, seed)?;

    let birthday = make_birthday(network, birthday_height);

    let (account_id, usk) = with_wallet_db_write_lock("keys.create_account", || {
        let mut db = open_wallet_db_for_mutation(db_path, network)?;

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

/// Import a same-seed software account for a specific ZIP32 account index as a
/// derived account. This is used only after the first seed-anchor account has
/// initialized the wallet DB with the same seed.
/// The account receives the supplied name.
pub fn import_derived_account_at_index(
    db_path: &str,
    network: WalletNetwork,
    seed: &SecretVec<u8>,
    birthday_height: Option<u64>,
    account_index: u32,
    name: &str,
) -> Result<CreatedAccount, String> {
    let birthday = make_birthday(network, birthday_height);
    let account_id = zip32_account_id(account_index)?;
    let address = default_address_for_account(network, seed, account_index)?;

    let account = with_wallet_db_write_lock("keys.import_derived_account_at_index", || {
        let mut db = open_wallet_db_for_mutation(db_path, network)?;
        db.import_account_hd(name, seed, account_id, &birthday, None)
            .map(|(account, _usk)| account)
            .map_err(|e| format!("Failed to import derived account: {e}"))
    })?;

    Ok(CreatedAccount {
        uuid: account.id().expose_uuid().to_string(),
        unified_address: address,
    })
}

/// List the seed-derived accounts in the wallet database, including their UUIDs.
pub fn list_accounts(db_path: &str, network: WalletNetwork) -> Result<Vec<AccountInfo>, String> {
    let db = open_wallet_db_for_read(db_path, network)?;

    let account_ids = db
        .get_account_ids()
        .map_err(|e| format!("Failed to list accounts: {e}"))?;

    let mut accounts = Vec::new();
    for id in account_ids {
        let account = db
            .get_account(id)
            .map_err(|e| format!("Failed to get account: {e}"))?
            .ok_or_else(|| format!("Account not found: {}", id.expose_uuid()))?;

        let source = account.source();
        let Some(derivation) = source.key_derivation() else {
            continue;
        };

        let address = match account.ufvk() {
            Some(ufvk) => current_receive_address(&db, network, id, ufvk)?,
            None => String::new(),
        };

        accounts.push(AccountInfo {
            uuid: id.expose_uuid().to_string(),
            name: account.name().unwrap_or("").to_string(),
            unified_address: address,
            account_index: u32::from(derivation.account_index()),
            birthday_height: u32::from(account.birthday_height()),
            db_path: db_path.to_string(),
            network,
        });
    }

    accounts.sort_by_key(|a| a.account_index);
    Ok(accounts)
}

/// True when the wallet database has accounts derived from `seed`.
pub fn seed_is_in_wallet(
    db_path: &str,
    network: WalletNetwork,
    seed: &SecretVec<u8>,
) -> Result<bool, String> {
    let db = open_wallet_db_for_read(db_path, network)?;
    let relevance = db
        .seed_relevance_to_derived_accounts(seed)
        .map_err(|e| format!("Failed to check recovery phrase: {e}"))?;
    Ok(matches!(
        relevance,
        zcash_client_backend::data_api::SeedRelevance::Relevant { .. }
    ))
}

/// Parse an account UUID string into the backend's AccountUuid.
pub fn parse_account_uuid(s: &str) -> Result<AccountUuid, String> {
    let uuid = uuid::Uuid::parse_str(s).map_err(|e| format!("Invalid account UUID: {e}"))?;
    Ok(AccountUuid::from_uuid(uuid))
}

fn current_receive_address(
    db: &WalletDatabase,
    network: WalletNetwork,
    account_id: AccountUuid,
    ufvk: &UnifiedFullViewingKey,
) -> Result<String, String> {
    let (default, _) = ufvk
        .default_address(shielded_address_request())
        .map_err(|e| format!("Failed to derive shielded address: {e}"))?;

    let last = db
        .get_last_generated_address_matching(account_id, shielded_address_request())
        .map_err(|e| format!("Failed to get last generated shielded address: {e}"))?;

    let address = last.unwrap_or(default);
    Ok(address.encode(&network))
}

/// zorg's data directory: `~/Library/Application Support/zorg`.
pub fn wallet_dir() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home).join("Library/Application Support/zorg"))
}

/// Opens `zorg_index.db` and holds its write lock until `COMMIT`.
pub fn open_index_for_write(root: &Path) -> Result<rusqlite::Connection, String> {
    ensure_private_wallet_dir(root)?;
    let path = root.join("zorg_index.db");
    let conn = rusqlite::Connection::open(&path)
        .map_err(|e| format!("Could not open database index: {e}"))?;
    secure_wallet_db_file(&path)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS databases (db_path TEXT PRIMARY KEY NOT NULL);
         BEGIN IMMEDIATE;",
    )
    .map_err(|e| format!("Could not lock database index: {e}"))?;
    Ok(conn)
}

/// The database paths in an open `zorg_index.db`, in the order they were added.
pub fn database_paths(conn: &rusqlite::Connection) -> Result<Vec<PathBuf>, String> {
    let mut stmt = conn
        .prepare("SELECT db_path FROM databases ORDER BY rowid")
        .map_err(|e| format!("Could not read database index: {e}"))?;
    let paths = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
        .map_err(|e| format!("Could not read database index: {e}"))?;
    Ok(paths.into_iter().map(PathBuf::from).collect())
}

/// The database paths in `zorg_index.db`, or none when it does not exist.
pub fn read_database_paths(root: &Path) -> Result<Vec<PathBuf>, String> {
    let path = root.join("zorg_index.db");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("Could not open database index: {e}"))?;
    database_paths(&conn)
}

/// Adds a database path to an open `zorg_index.db`.
pub fn add_database_path(conn: &rusqlite::Connection, path: &Path) -> Result<(), String> {
    let path = path.to_str().ok_or("Database path must be UTF-8")?;
    conn.execute("INSERT INTO databases (db_path) VALUES (?1)", [path])
        .map_err(|e| format!("Could not update database index: {e}"))?;
    Ok(())
}

/// Removes a database path from an open `zorg_index.db`.
pub fn remove_database_path(conn: &rusqlite::Connection, path: &Path) -> Result<(), String> {
    let path = path.to_str().ok_or("Database path must be UTF-8")?;
    conn.execute("DELETE FROM databases WHERE db_path = ?1", [path])
        .map_err(|e| format!("Could not update database index: {e}"))?;
    Ok(())
}

/// Adds a new wallet database to `zorg_index.db` after checking that its
/// account name and seed are still unused.
pub fn index_new_database(
    root: &Path,
    db_path: &Path,
    name: &str,
    seed: &SecretVec<u8>,
    network: WalletNetwork,
) -> Result<(), String> {
    let index = open_index_for_write(root)?;
    let databases = database_paths(&index)?;
    let (accounts, unavailable) = accounts_from_databases(&databases)?;
    require_available(&unavailable)?;
    ensure_unique_name(&accounts, name)?;
    ensure_new_seed(&databases, seed, network)?;
    add_database_path(&index, db_path)?;
    index
        .execute_batch("COMMIT")
        .map_err(|e| format!("Could not update database index: {e}"))
}

/// Deletes `account` by removing its wallet database from `zorg_index.db` and
/// deleting the database file. Fails without deleting anything unless the
/// database is in `root` and holds exactly this account.
pub fn delete_account(root: &Path, account: &AccountInfo) -> Result<(), String> {
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
    match list_accounts(db_path, account.network)?.as_slice() {
        [only] if only.uuid == account.uuid => {}
        _ => {
            return Err(format!(
                "{db_path} does not hold exactly account {:?}; not deleting it",
                account.name
            ))
        }
    }
    remove_database_path(&index, Path::new(db_path))?;
    index
        .execute_batch("COMMIT")
        .map_err(|e| format!("Could not update database index: {e}"))?;
    remove_wallet_files(db_path);
    Ok(())
}

/// Removes a wallet database and its SQLite side files, ignoring missing ones.
pub fn remove_wallet_files(db: &str) {
    for path in [db.to_string(), format!("{db}-wal"), format!("{db}-shm")] {
        let _ = std::fs::remove_file(path);
    }
}

/// The accounts in the given databases, and a message for each database that is unavailable.
pub fn accounts_from_databases(
    databases: &[PathBuf],
) -> Result<(Vec<AccountInfo>, Vec<String>), String> {
    let mut result = Vec::new();
    let mut unavailable = Vec::new();
    let mut seen_paths = std::collections::HashSet::new();
    for path in databases {
        let canonical = match path.canonicalize() {
            Ok(path) => path,
            Err(e) => {
                unavailable.push(format!("Database unavailable: {} ({e})", path.display()));
                continue;
            }
        };
        if !seen_paths.insert(canonical) {
            continue;
        }
        match accounts_in_database(path) {
            Ok(accounts) => result.extend(accounts),
            Err(e) => unavailable.push(format!("Database unavailable: {} ({e})", path.display())),
        }
    }
    Ok((result, unavailable))
}

/// The accounts in one wallet database.
fn accounts_in_database(path: &Path) -> Result<Vec<AccountInfo>, String> {
    let path = path.to_str().ok_or("Database path must be UTF-8")?;
    let Some(net) = network_in_wallet_db(path)? else {
        return Ok(Vec::new());
    };
    ensure_custom_wallet_dir(Path::new(path).parent().ok_or("Database has no parent")?)?;
    secure_wallet_db_file(Path::new(path))?;
    list_accounts(path, net)
}

/// Fails when one of `databases` already holds accounts from `seed` on `network`.
pub fn ensure_new_seed(
    databases: &[PathBuf],
    seed: &secrecy::SecretVec<u8>,
    network: WalletNetwork,
) -> Result<(), String> {
    for path in databases {
        let path = path.to_str().ok_or("Database path must be UTF-8")?;
        if network_in_wallet_db(path)? == Some(network) && seed_is_in_wallet(path, network, seed)? {
            return Err(
                "This recovery phrase is already known on this network; run `zorg accounts`".into(),
            );
        }
    }
    Ok(())
}

/// Fails when any database is unavailable.
pub fn require_available(unavailable: &[String]) -> Result<(), String> {
    if unavailable.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Reconnect unavailable databases before adding accounts:\n{}",
            unavailable.join("\n")
        ))
    }
}

/// Fails unless `name` is nonempty, with no surrounding whitespace or control characters.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.trim() != name || name.chars().any(char::is_control) {
        Err(
            "Account name must be nonempty, with no surrounding whitespace or control characters"
                .into(),
        )
    } else {
        Ok(())
    }
}

/// The account named `name`.
pub fn select<'a>(
    accounts: &'a [AccountInfo],
    name: Option<&str>,
) -> Result<&'a AccountInfo, String> {
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
pub fn ensure_unique_name(accounts: &[AccountInfo], name: &str) -> Result<(), String> {
    validate_name(name)?;
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

/// Creates `path` if needed and fails when other users can access it.
pub fn ensure_custom_wallet_dir(path: &std::path::Path) -> Result<(), String> {
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

/// Sets an existing wallet file's permissions to 0600.
pub fn secure_wallet_db_file(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    if path.exists() {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("Failed to secure wallet database: {e}"))?;
    }
    Ok(())
}

/// The network of the wallet database at `db_path`, or none when it has no accounts.
pub fn network_in_wallet_db(db_path: &str) -> Result<Option<WalletNetwork>, String> {
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
    let (network, _, _) = zcash_address::unified::Uivk::decode(&encoded_key).map_err(|e| {
        format!("Could not determine wallet database network from account key: {e}")
    })?;
    use zcash_protocol::consensus::NetworkType;
    Ok(Some(match network {
        NetworkType::Main => WalletNetwork::Main,
        NetworkType::Test => WalletNetwork::Test,
        NetworkType::Regtest => WalletNetwork::Regtest,
    }))
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

        // Mainnet unified addresses start with "u1"
        assert!(
            address.starts_with("u1"),
            "Expected u1 prefix, got: {address}"
        );

        // Verify we can read the address back
        let address2 = list_accounts(db_path_str, WalletNetwork::Main).unwrap()[0]
            .unified_address
            .clone();
        assert_eq!(address, address2);
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
        let uuid = created.uuid;
        let default_address = created.unified_address;

        crate::sync::update_chain_tip(db_path_str, WalletNetwork::Main, 2_500_000).unwrap();
        let renewed_address = crate::sync::get_next_available_address(
            db_path_str,
            WalletNetwork::Main,
            &uuid,
            crate::sync::AddressRequestKind::Shielded,
        )
        .unwrap();

        assert_ne!(default_address, renewed_address);
        assert_eq!(
            renewed_address,
            list_accounts(db_path_str, WalletNetwork::Main).unwrap()[0].unified_address
        );
    }

    #[test]
    fn network_in_wallet_db_reads_the_network_from_an_account() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("wallet.db");
        let db_path_str = db_path.to_str().unwrap();
        assert_eq!(network_in_wallet_db(db_path_str).unwrap(), None);

        ensure_db_initialized(db_path_str, WalletNetwork::Test).unwrap();
        assert_eq!(network_in_wallet_db(db_path_str).unwrap(), None);

        let seed = mnemonic_to_seed(&generate_mnemonic()).unwrap();
        init_db_and_create_account(db_path_str, WalletNetwork::Test, &seed, None, "Account 1")
            .unwrap();
        assert_eq!(
            network_in_wallet_db(db_path_str).unwrap(),
            Some(WalletNetwork::Test)
        );
    }

    #[test]
    fn a_broken_database_is_unavailable_without_hiding_the_others() {
        let temp_dir = tempfile::tempdir().unwrap();
        ensure_private_wallet_dir(temp_dir.path()).unwrap();
        let good = temp_dir.path().join("good.db");
        let seed = mnemonic_to_seed(&generate_mnemonic()).unwrap();
        init_db_and_create_account(
            good.to_str().unwrap(),
            WalletNetwork::Test,
            &seed,
            None,
            "Daily",
        )
        .unwrap();
        let broken = temp_dir.path().join("broken.db");
        std::fs::write(&broken, b"not a wallet database").unwrap();

        let (accounts, unavailable) = accounts_from_databases(&[broken, good]).unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].name, "Daily");
        assert_eq!(unavailable.len(), 1);
        assert!(unavailable[0].contains("broken.db"));
    }

    #[test]
    fn test_deterministic_address_from_same_seed() {
        let phrase = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon art";
        let seed = mnemonic_to_seed(phrase).unwrap();

        let temp1 = tempfile::tempdir().unwrap();
        let db1 = temp1.path().join("wallet.db");
        let addr1 = init_db_and_create_account(
            db1.to_str().unwrap(),
            WalletNetwork::Main,
            &seed,
            None,
            "Account 1",
        )
        .unwrap()
        .unified_address;

        let temp2 = tempfile::tempdir().unwrap();
        let db2 = temp2.path().join("wallet.db");
        let addr2 = init_db_and_create_account(
            db2.to_str().unwrap(),
            WalletNetwork::Main,
            &seed,
            None,
            "Account 1",
        )
        .unwrap()
        .unified_address;

        assert_eq!(addr1, addr2, "Same seed should produce same address");
    }

    #[test]
    fn test_shielded_address_has_sapling_and_orchard_only() {
        // Verify our address uses Sapling+Orchard receivers (no transparent),
        // matching zodl/Zashi wallet behavior.
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("wallet.db");
        let db_path_str = db_path.to_str().unwrap();

        let phrase = generate_mnemonic();
        let seed = mnemonic_to_seed(&phrase).unwrap();

        let address =
            init_db_and_create_account(db_path_str, WalletNetwork::Main, &seed, None, "Account 1")
                .unwrap()
                .unified_address;
        // Decode and verify receiver types
        let za = zcash_address::ZcashAddress::try_from_encoded(&address).unwrap();
        let debug = format!("{:?}", za);
        assert!(
            debug.contains("Sapling"),
            "UA should contain Sapling receiver"
        );
        assert!(
            debug.contains("Orchard"),
            "UA should contain Orchard receiver"
        );
        assert!(
            !debug.contains("P2pkh"),
            "UA should NOT contain transparent receiver"
        );
    }
}
