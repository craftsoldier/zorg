use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

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
/// This pins the DB seed fingerprint for seed-aware initialization.
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
    current_receive_address(&db, account.network, id, &ufvk)
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
        "CREATE TABLE IF NOT EXISTS accounts (
             uuid TEXT PRIMARY KEY NOT NULL,
             name TEXT NOT NULL UNIQUE,
             network TEXT NOT NULL,
             db_path TEXT NOT NULL UNIQUE
         );
         BEGIN IMMEDIATE;",
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
    let path = root.join("zorg_index.db");
    if !path.exists() {
        return Ok(Vec::new());
    }
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("Could not open database index: {e}"))?;
    indexed_accounts(&conn)
}

/// Adds `account` to `zorg_index.db` after checking that its name and seed
/// are still unused.
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
        .execute_batch("COMMIT")
        .map_err(|e| format!("Could not update database index: {e}"))
}

/// Deletes `account` from `zorg_index.db` along with its database files, if
/// any are left. Fails without deleting anything unless the database is in
/// `root` and, when it exists, holds exactly this account.
pub fn delete_account(root: &Path, account: &Account) -> Result<(), String> {
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
    // Dropping `index` without COMMIT keeps the row when a file can't be deleted.
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
        if seed_is_in_wallet(&account.db_path, network, seed)? {
            return Err(
                "This recovery phrase is already known on this network; run `zorg accounts`".into(),
            );
        }
    }
    Ok(())
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

        // Mainnet unified addresses start with "u1"
        assert!(
            address.starts_with("u1"),
            "Expected u1 prefix, got: {address}"
        );

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
            init_db_and_create_account(db_str, WalletNetwork::Regtest, &seed, Some(1), "A")
                .unwrap();
        let account = Account {
            uuid: created.uuid,
            name: "A".into(),
            network: WalletNetwork::Regtest,
            db_path: db_str.into(),
        };
        index_new_account(root.path(), &account, &seed).unwrap();
        let chflags = |flag: &str| {
            assert!(std::process::Command::new("chflags")
                .args([flag, db_str])
                .status()
                .unwrap()
                .success());
        };

        chflags("uchg");
        let error = delete_account(root.path(), &account);
        chflags("nouchg");
        assert!(error.unwrap_err().contains("Could not delete"));
        assert!(db.exists());
        assert_eq!(
            read_index(root.path()).unwrap(),
            std::slice::from_ref(&account)
        );

        delete_account(root.path(), &account).unwrap();
        assert!(!db.exists());
        assert!(read_index(root.path()).unwrap().is_empty());
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
