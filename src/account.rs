use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

use rusqlite::{named_params, OptionalExtension};
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

/// Delete an account from the wallet database.
pub fn delete_account(
    db_path: &str,
    network: WalletNetwork,
    account_uuid: &str,
) -> Result<(), String> {
    let account_id = parse_account_uuid(account_uuid)?;
    with_wallet_db_write_lock("keys.delete_account", || {
        let db = open_wallet_db_for_mutation(db_path, network)?;
        db.get_account(account_id)
            .map_err(|e| format!("Failed to load account: {e}"))?
            .ok_or_else(|| format!("Account not found: {}", account_id.expose_uuid()))?;

        // zcash_client_sqlite 0.19.5 has a named-parameter bug in
        // wallet::delete_account: the sent_notes rewrite binds `:address`
        // while the SQL expects `:to_address`. Keep this local copy aligned
        // with upstream except for that binding until the dependency is fixed.
        drop(db);
        delete_account_rows(db_path, account_id)?;
        Ok(())
    })
}

fn delete_account_rows(db_path: &str, account_id: AccountUuid) -> Result<(), String> {
    let mut conn = rusqlite::Connection::open(db_path)
        .map_err(|e| format!("Failed to open wallet DB: {e}"))?;
    conn.busy_timeout(ACCOUNT_MUTATION_DB_BUSY_TIMEOUT)
        .map_err(|e| format!("Failed to configure wallet DB busy timeout: {e}"))?;
    rusqlite::vtab::array::load_module(&conn)
        .map_err(|e| format!("Failed to load SQLite array module: {e}"))?;
    conn.execute("PRAGMA foreign_keys = ON", [])
        .map_err(|e| format!("Failed to enable SQLite foreign keys: {e}"))?;

    let tx = conn
        .transaction()
        .map_err(|e| format!("Failed to begin account delete transaction: {e}"))?;
    let account_uuid = account_id.expose_uuid();
    let account_uuid_bytes = account_uuid.as_bytes().as_slice();

    {
        let mut to_account_tx = tx
            .prepare(
                r#"
                SELECT
                    sn.id AS sent_note_id,
                    COALESCE(addresses.address, addresses.cached_transparent_receiver_address) AS to_address
                FROM sent_notes sn
                JOIN v_received_outputs ro ON ro.sent_note_id = sn.id
                JOIN addresses ON addresses.id = ro.address_id
                JOIN accounts ta ON ta.id = sn.to_account_id
                WHERE ta.uuid = :account_uuid
                "#,
            )
            .map_err(|e| format!("Failed to prepare sent note rewrite query: {e}"))?;

        let mut update_sent_note = tx
            .prepare(
                r#"
                UPDATE sent_notes
                SET to_address = :to_address, to_account_id = NULL
                WHERE id = :sent_note_id
                "#,
            )
            .map_err(|e| format!("Failed to prepare sent note rewrite update: {e}"))?;

        let mut rows = to_account_tx
            .query(named_params![":account_uuid": account_uuid_bytes])
            .map_err(|e| format!("Failed to query sent notes for account deletion: {e}"))?;
        while let Some(row) = rows
            .next()
            .map_err(|e| format!("Failed to read sent notes for account deletion: {e}"))?
        {
            if let Some(address) = row
                .get::<_, Option<String>>("to_address")
                .map_err(|e| format!("Failed to read sent note destination address: {e}"))?
            {
                update_sent_note
                    .execute(named_params![
                        ":sent_note_id": row
                            .get::<_, i64>("sent_note_id")
                            .map_err(|e| format!("Failed to read sent note id: {e}"))?,
                        ":to_address": address,
                    ])
                    .map_err(|e| format!("Failed to rewrite sent note destination: {e}"))?;
            }
        }
    }

    tx.execute(
        r#"
        WITH account_transactions AS (
            SELECT ro.transaction_id
            FROM v_received_outputs ro
            JOIN accounts a ON a.id = ro.account_id
            WHERE a.uuid = :account_uuid
            UNION
            SELECT ros.transaction_id
            FROM v_received_output_spends ros
            JOIN accounts sa ON sa.id = ros.account_id
            WHERE sa.uuid = :account_uuid
        ),
        non_account_transactions AS (
            SELECT ro.transaction_id
            FROM v_received_outputs ro
            JOIN accounts a ON a.id = ro.account_id
            WHERE a.uuid != :account_uuid
            UNION
            SELECT ros.transaction_id
            FROM v_received_output_spends ros
            JOIN accounts sa ON sa.id = ros.account_id
            WHERE sa.uuid != :account_uuid
        )
        DELETE FROM transactions WHERE id_tx IN (
            SELECT transaction_id FROM account_transactions
            EXCEPT
            SELECT transaction_id FROM non_account_transactions
        )
        "#,
        named_params![":account_uuid": account_uuid_bytes],
    )
    .map_err(|e| format!("Failed to delete account-only transactions: {e}"))?;

    tx.execute(
        "DELETE FROM accounts WHERE uuid = :account_uuid",
        named_params![":account_uuid": account_uuid_bytes],
    )
    .map_err(|e| format!("Failed to delete account: {e}"))?;

    // Restore the "below the wallet birthday = Ignored" invariant for any
    // historical scan range that only the just-deleted account required. See
    // `prune_orphaned_scan_ranges_in_conn` for the full rationale (VZR-89).
    prune_orphaned_scan_ranges_in_conn(&tx)
        .map_err(|e| format!("Failed to prune orphaned scan ranges after deletion: {e}"))?;

    tx.commit()
        .map_err(|e| format!("Failed to commit account deletion: {e}"))
}

/// Demote orphaned historical scan ranges that lie below the remaining
/// accounts' minimum birthday back to `Ignored`.
///
/// librustzcash never garbage-collects `scan_queue` when an account is deleted
/// (neither upstream `delete_account` nor our `delete_account_rows`), and
/// importing an account with an old birthday force-rescans the whole chain,
/// demoting already-`Scanned` history to `Historic`. When that imported account
/// is later removed, the historical range it required is left pending in
/// `scan_queue` even though no remaining account needs it. The sync engine's
/// progress denominator is the sum of all pending ranges, so that orphan pins
/// progress near 0% and wastes hours re-scanning irrelevant blocks (VZR-89).
///
/// This restores the normal "below the wallet birthday = `Ignored`" invariant:
/// EVERY non-`Ignored` range strictly below `MIN(birthday_height)` is set to
/// `Ignored` — both leftover `Historic` (never-scanned orphan) AND leftover
/// `Scanned` ranges. The `Scanned` case matters: a deleted old-birthday account
/// that was only partially synced leaves a `Scanned` range below the surviving
/// birthday, and librustzcash's `block_fully_scanned` takes the *first*
/// `Scanned` range starting at/below the birthday and reports its end as the
/// wallet's fully-scanned height. If that leftover `Scanned` range is left in
/// place, an `Ignored` gap above it pins the fully-scanned height there forever
/// and sync can never complete (`ensure_complete_scan_state` blocks it). Scan
/// ranges are disjoint, so at most one range straddles the birthday; it is split
/// so the portion at/above the birthday keeps its priority. Ranges at/above the
/// birthday (including the live chain-tip gap) are never touched.
///
/// Idempotent and a no-op for healthy wallets (their sub-birthday range is
/// already `Ignored`), so it is safe to call on every sync start as a rescue
/// for wallets already stuck by a pre-fix deletion. Returns the number of
/// `scan_queue` rows whose coverage was demoted (a split counts once).
fn prune_orphaned_scan_ranges_in_conn(conn: &rusqlite::Connection) -> rusqlite::Result<usize> {
    // scan_queue priority code for `Ignored` (zcash_client_backend ScanPriority).
    // We demote anything ABOVE this (Scanned=10, Historic=20, ...) that sits
    // below the birthday, so the whole sub-birthday region becomes uniformly
    // Ignored — matching the shape librustzcash produces for a fresh wallet.
    const PRIORITY_IGNORED: i64 = 0;

    let min_birthday: Option<i64> =
        conn.query_row("SELECT MIN(birthday_height) FROM accounts", [], |row| {
            row.get(0)
        })?;
    let Some(min_birthday) = min_birthday else {
        // No accounts remain (full reset handles that path); nothing to prune.
        return Ok(0);
    };

    let mut demoted = 0usize;

    // Split the single non-Ignored range (if any) that straddles the birthday:
    // [start, min_birthday) becomes Ignored, [min_birthday, end) keeps priority.
    let straddling_start: Option<i64> = conn
        .query_row(
            "SELECT block_range_start FROM scan_queue \
             WHERE block_range_start < :b AND block_range_end > :b AND priority > :ignored \
             LIMIT 1",
            named_params![":b": min_birthday, ":ignored": PRIORITY_IGNORED],
            |row| row.get(0),
        )
        .optional()?;

    if let Some(start) = straddling_start {
        // Shrink the existing row to [min_birthday, end), preserving its
        // priority. Ranges are disjoint, so `min_birthday` is free as a new
        // start bound (no other row can start at or span it).
        conn.execute(
            "UPDATE scan_queue SET block_range_start = :b WHERE block_range_start = :start",
            named_params![":b": min_birthday, ":start": start],
        )?;
        // Insert the below-birthday remainder as Ignored. `start` was just
        // freed, and `min_birthday` is free as an end bound (disjoint invariant).
        conn.execute(
            "INSERT INTO scan_queue (block_range_start, block_range_end, priority) \
             VALUES (:start, :b, :ignored)",
            named_params![":start": start, ":b": min_birthday, ":ignored": PRIORITY_IGNORED],
        )?;
        demoted += 1;
    }

    // Demote every non-Ignored range that lies entirely below the birthday
    // (orphaned Historic AND leftover Scanned from a deleted account's partial
    // sync) so no Scanned range remains below the birthday to confuse
    // `block_fully_scanned`.
    demoted += conn.execute(
        "UPDATE scan_queue SET priority = :ignored \
         WHERE block_range_end <= :b AND priority > :ignored",
        named_params![":ignored": PRIORITY_IGNORED, ":b": min_birthday],
    )?;

    Ok(demoted)
}

/// Path-based wrapper around [`prune_orphaned_scan_ranges_in_conn`] for callers
/// that are not already inside a wallet-DB write transaction (e.g. the sync
/// engine's startup rescue pass). Acquires the wallet-DB write lock, runs the
/// prune in its own transaction, and commits. Returns the number of ranges
/// demoted.
pub fn prune_orphaned_scan_ranges(db_path: &str) -> Result<usize, String> {
    with_wallet_db_write_lock("keys.prune_orphaned_scan_ranges", || {
        let mut conn = rusqlite::Connection::open(db_path)
            .map_err(|e| format!("Failed to open wallet DB: {e}"))?;
        conn.busy_timeout(ACCOUNT_MUTATION_DB_BUSY_TIMEOUT)
            .map_err(|e| format!("Failed to configure wallet DB busy timeout: {e}"))?;
        let tx = conn
            .transaction()
            .map_err(|e| format!("Failed to begin scan-queue prune transaction: {e}"))?;
        let demoted = prune_orphaned_scan_ranges_in_conn(&tx)
            .map_err(|e| format!("Failed to prune orphaned scan ranges: {e}"))?;
        tx.commit()
            .map_err(|e| format!("Failed to commit scan-queue prune: {e}"))?;
        Ok(demoted)
    })
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

/// zorg's data directory: `~/Library/Application Support/zorg` on macOS.
pub fn wallet_dir() -> Result<PathBuf, String> {
    let home = std::env::var_os("HOME").ok_or("HOME is not set")?;
    let home = PathBuf::from(home);
    Ok(if cfg!(target_os = "macos") {
        home.join("Library/Application Support/zorg")
    } else {
        home.join(".zorg")
    })
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

/// Deletes an account, and its wallet database and index row when it was the
/// database's last account. Returns whether the database was deleted.
pub fn delete_account_and_empty_database(
    root: &Path,
    db_path: &str,
    network: WalletNetwork,
    account_uuid: &str,
) -> Result<bool, String> {
    delete_account(db_path, network, account_uuid)?;
    if !list_accounts(db_path, network)?.is_empty() {
        return Ok(false);
    }
    let index = open_index_for_write(root)?;
    remove_database_path(&index, Path::new(db_path))?;
    index
        .execute_batch("COMMIT")
        .map_err(|e| format!("Could not update database index: {e}"))?;
    remove_wallet_files(db_path);
    Ok(true)
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
#[cfg(unix)]
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
#[cfg(unix)]
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

#[cfg(not(unix))]
pub fn ensure_private_wallet_dir(path: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|e| format!("Failed to create wallet directory: {e}"))
}

#[cfg(not(unix))]
pub fn ensure_custom_wallet_dir(path: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(path).map_err(|e| format!("Failed to create wallet directory: {e}"))
}

/// Sets an existing wallet file's permissions to 0600.
#[cfg(unix)]
pub fn secure_wallet_db_file(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;

    if path.exists() {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("Failed to secure wallet database: {e}"))?;
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn secure_wallet_db_file(_path: &std::path::Path) -> Result<(), String> {
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
    fn test_delete_account_removes_account_from_wallet_db() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("wallet.db");
        let db_path_str = db_path.to_str().unwrap();

        let first_phrase = generate_mnemonic();
        let first_seed = mnemonic_to_seed(&first_phrase).unwrap();
        init_db_and_create_account(
            db_path_str,
            WalletNetwork::Main,
            &first_seed,
            None,
            "Account 1",
        )
        .unwrap();

        // Second numbered account, derived from the same seed at index 1.
        let second_uuid = import_derived_account_at_index(
            db_path_str,
            WalletNetwork::Main,
            &first_seed,
            None,
            1,
            "Account 2",
        )
        .unwrap()
        .uuid;

        assert_eq!(
            list_accounts(db_path_str, WalletNetwork::Main)
                .unwrap()
                .len(),
            2
        );

        delete_account(db_path_str, WalletNetwork::Main, &second_uuid).unwrap();

        let accounts = list_accounts(db_path_str, WalletNetwork::Main).unwrap();
        assert_eq!(accounts.len(), 1);
        assert!(accounts.iter().all(|account| account.name != "second"));
    }

    // --- VZR-89: orphaned scan-range pruning -------------------------------

    fn scan_min_birthday(db_path: &str) -> i64 {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.query_row("SELECT MIN(birthday_height) FROM accounts", [], |r| {
            r.get(0)
        })
        .unwrap()
    }

    fn pending_scan_coverage_below(db_path: &str, height: i64) -> bool {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM scan_queue \
             WHERE block_range_start < :h AND priority > 10)",
            named_params![":h": height],
            |r| r.get(0),
        )
        .unwrap()
    }

    fn pending_scan_coverage_at_or_above(db_path: &str, height: i64) -> bool {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM scan_queue \
             WHERE block_range_end > :h AND priority > 10)",
            named_params![":h": height],
            |r| r.get(0),
        )
        .unwrap()
    }

    /// Reproduces the issue #272 / VZR-89 sequence end-to-end: an existing
    /// recent-birthday wallet, an additional import with an old (Sapling
    /// activation) birthday that force-rescans the whole chain, then deletion
    /// of that import. After deletion no pending scan range may remain below the
    /// surviving account's birthday, while its own near-tip range is preserved.
    #[test]
    fn test_delete_account_prunes_orphaned_scan_ranges_below_birthday() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("wallet.db");
        let db_path_str = db_path.to_str().unwrap();

        let existing_seed = mnemonic_to_seed(&generate_mnemonic()).unwrap();
        init_db_and_create_account(
            db_path_str,
            WalletNetwork::Main,
            &existing_seed,
            Some(2_400_000),
            "Account 1",
        )
        .unwrap();
        crate::sync::update_chain_tip(db_path_str, WalletNetwork::Main, 2_500_000).unwrap();

        // Capture the surviving account's birthday BEFORE the import: this is
        // the threshold the orphan must end up below. (MIN(birthday) read after
        // the import would include the imported account's old birthday.)
        let surviving_birthday = scan_min_birthday(db_path_str);

        let imported_uuid = import_derived_account_at_index(
            db_path_str,
            WalletNetwork::Main,
            &existing_seed,
            Some(419_200),
            1,
            "Account 2",
        )
        .unwrap()
        .uuid;

        // Sanity: importing the old-birthday account force-rescanned the chain,
        // queuing pending (Historic) coverage below the surviving birthday.
        assert!(
            pending_scan_coverage_below(db_path_str, surviving_birthday),
            "old-birthday import should queue a historical range below the existing birthday",
        );

        delete_account(db_path_str, WalletNetwork::Main, &imported_uuid).unwrap();

        assert!(
            !pending_scan_coverage_below(db_path_str, surviving_birthday),
            "deleting the old-birthday account must prune the orphaned historical scan range",
        );
        assert!(
            pending_scan_coverage_at_or_above(db_path_str, surviving_birthday),
            "the surviving account's own near-tip scan range must be preserved",
        );
    }

    /// Directly exercises the standalone rescue used at sync start: a wallet
    /// already stuck by a pre-fix deletion (orphaned pending ranges injected
    /// below the birthday, including one straddling range) is healed by
    /// `prune_orphaned_scan_ranges`, and the pass is idempotent.
    #[test]
    fn test_prune_orphaned_scan_ranges_rescues_stuck_wallet_and_splits_straddling() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("wallet.db");
        let db_path_str = db_path.to_str().unwrap();

        let seed = mnemonic_to_seed(&generate_mnemonic()).unwrap();
        init_db_and_create_account(
            db_path_str,
            WalletNetwork::Main,
            &seed,
            Some(2_400_000),
            "Account 1",
        )
        .unwrap();
        crate::sync::update_chain_tip(db_path_str, WalletNetwork::Main, 2_500_000).unwrap();

        let min_birthday = scan_min_birthday(db_path_str);
        let below_start = min_birthday - 2_000_000;
        let below_end = min_birthday - 1_000_000;
        let straddle_end = min_birthday + 50_000;
        let tip_end = min_birthday + 100_000;

        // Simulate the leftover stuck state explicitly so the test does not
        // depend on the on-delete pruning: a fully-below Historic orphan, a
        // Historic range straddling the birthday, and a legitimate near-tip
        // ChainTip range above the birthday.
        {
            let conn = rusqlite::Connection::open(db_path_str).unwrap();
            conn.execute("DELETE FROM scan_queue", []).unwrap();
            conn.execute(
                "INSERT INTO scan_queue (block_range_start, block_range_end, priority) \
                 VALUES (:s, :e, 20)",
                named_params![":s": below_start, ":e": below_end],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO scan_queue (block_range_start, block_range_end, priority) \
                 VALUES (:s, :e, 20)",
                named_params![":s": below_end, ":e": straddle_end],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO scan_queue (block_range_start, block_range_end, priority) \
                 VALUES (:s, :e, 50)",
                named_params![":s": straddle_end, ":e": tip_end],
            )
            .unwrap();
        }

        let demoted = prune_orphaned_scan_ranges(db_path_str).unwrap();
        assert!(
            demoted >= 1,
            "expected to demote at least the straddling range"
        );

        // Rescue: nothing pending remains below the birthday.
        assert!(!pending_scan_coverage_below(db_path_str, min_birthday));

        let conn = rusqlite::Connection::open(db_path_str).unwrap();
        // Straddling range split: [birthday, straddle_end) preserved as Historic.
        let upper_kept: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM scan_queue \
                 WHERE block_range_start = :b AND block_range_end = :e AND priority = 20)",
                named_params![":b": min_birthday, ":e": straddle_end],
                |r| r.get(0),
            )
            .unwrap();
        assert!(upper_kept, "split must keep [birthday, end) as Historic");
        // ...and its below-birthday remainder became Ignored.
        let lower_ignored: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM scan_queue \
                 WHERE block_range_start = :s AND block_range_end = :b AND priority = 0)",
                named_params![":s": below_end, ":b": min_birthday],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            lower_ignored,
            "split must Ignore the [start, birthday) remainder"
        );
        // Near-tip range above the birthday is untouched.
        let tip_untouched: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM scan_queue \
                 WHERE block_range_start = :s AND block_range_end = :e AND priority = 50)",
                named_params![":s": straddle_end, ":e": tip_end],
                |r| r.get(0),
            )
            .unwrap();
        assert!(tip_untouched, "ranges above the birthday must be preserved");
        drop(conn);

        // Idempotent: a second pass demotes nothing.
        assert_eq!(prune_orphaned_scan_ranges(db_path_str).unwrap(), 0);
    }

    fn scan_queue_snapshot(db_path: &str) -> Vec<(i64, i64, i64)> {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT block_range_start, block_range_end, priority \
                 FROM scan_queue ORDER BY block_range_start",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    /// The prune must be a strict no-op on a healthy, librustzcash-produced
    /// wallet (one that never imported+deleted an old-birthday account): its
    /// sub-birthday range is already `Ignored`, so nothing is pending below the
    /// birthday. This pins the docstring's "no-op for healthy wallets" contract
    /// directly, since the prune runs on every sync start for every user.
    #[test]
    fn test_prune_orphaned_scan_ranges_is_noop_on_healthy_wallet() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("wallet.db");
        let db_path_str = db_path.to_str().unwrap();

        let seed = mnemonic_to_seed(&generate_mnemonic()).unwrap();
        init_db_and_create_account(
            db_path_str,
            WalletNetwork::Main,
            &seed,
            Some(2_400_000),
            "Account 1",
        )
        .unwrap();
        crate::sync::update_chain_tip(db_path_str, WalletNetwork::Main, 2_500_000).unwrap();

        let before = scan_queue_snapshot(db_path_str);
        let demoted = prune_orphaned_scan_ranges(db_path_str).unwrap();
        let after = scan_queue_snapshot(db_path_str);

        assert_eq!(
            demoted, 0,
            "a healthy wallet has no pending coverage below its birthday to demote",
        );
        assert_eq!(
            before, after,
            "scan_queue must be byte-for-byte unchanged on a healthy wallet",
        );
    }

    fn lowest_scanned_range_start(db_path: &str) -> Option<i64> {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.query_row(
            "SELECT block_range_start FROM scan_queue \
             WHERE priority = 10 ORDER BY block_range_start ASC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
    }

    /// Regression for the real-device failure: a deleted old-birthday account
    /// that was only PARTIALLY synced leaves a `Scanned` range below the
    /// surviving account's birthday. The prune must demote that leftover
    /// `Scanned` range to `Ignored` too — otherwise librustzcash's
    /// `block_fully_scanned` keys off it (first Scanned range) and, with the
    /// Ignored gap the prune creates above it, pins the fully-scanned height
    /// there forever, blocking sync completion
    /// ("fully scanned height N below wallet DB chain tip").
    #[test]
    fn test_prune_orphaned_scan_ranges_demotes_leftover_scanned_below_birthday() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("wallet.db");
        let db_path_str = db_path.to_str().unwrap();

        let seed = mnemonic_to_seed(&generate_mnemonic()).unwrap();
        init_db_and_create_account(
            db_path_str,
            WalletNetwork::Main,
            &seed,
            Some(2_400_000),
            "Account 1",
        )
        .unwrap();
        crate::sync::update_chain_tip(db_path_str, WalletNetwork::Main, 2_500_000).unwrap();

        let birthday = scan_min_birthday(db_path_str);

        // Reproduce the post-delete state from a partially-synced old-birthday
        // import: a leftover Scanned range below the birthday, plus a Historic
        // remainder that straddles the birthday.
        {
            let conn = rusqlite::Connection::open(db_path_str).unwrap();
            conn.execute("DELETE FROM scan_queue", []).unwrap();
            conn.execute(
                "INSERT INTO scan_queue (block_range_start, block_range_end, priority) \
                 VALUES (419200, 746400, 10)", // Scanned, fully below birthday
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO scan_queue (block_range_start, block_range_end, priority) \
                 VALUES (746400, 2500001, 20)", // Historic, straddles the birthday
                [],
            )
            .unwrap();
        }

        // Before the fix this leftover Scanned range was left in place.
        assert_eq!(lowest_scanned_range_start(db_path_str), Some(419200));

        let demoted = prune_orphaned_scan_ranges(db_path_str).unwrap();
        assert!(demoted >= 1);

        // KEY: no Scanned range may remain below the birthday — otherwise
        // block_fully_scanned would pin the fully-scanned height to its end.
        match lowest_scanned_range_start(db_path_str) {
            None => {}
            Some(start) => assert!(
                start >= birthday,
                "a Scanned range must not remain below the birthday (found start {start} < {birthday})",
            ),
        }
        // The whole sub-birthday region is now Ignored; no pending coverage below.
        assert!(!pending_scan_coverage_below(db_path_str, birthday));
        // The straddle's at/above-birthday portion is preserved as pending.
        assert!(pending_scan_coverage_at_or_above(db_path_str, birthday));

        // Idempotent.
        assert_eq!(prune_orphaned_scan_ranges(db_path_str).unwrap(), 0);
    }

    /// Adversarial probe for state S14: the single straddling range is a
    /// HIGH-priority range (FoundNote=40), not Historic/Scanned. This exercises
    /// the load-bearing split-vs-demote dispatch on the straddler: the straddle
    /// SELECT uses `priority > Ignored`, so FoundNote must be caught by the
    /// split (preserving the above-birthday FoundNote coverage the survivor
    /// still needs), NOT blanket-demoted (which would Ignore a real note-bearing
    /// range above the birthday). The below-birthday remainder — a note-discovery
    /// hint that belonged only to the deleted account — is correctly Ignored.
    #[test]
    fn test_prune_splits_high_priority_foundnote_straddler_preserving_above_birthday() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("wallet.db");
        let db_path_str = db_path.to_str().unwrap();

        let seed = mnemonic_to_seed(&generate_mnemonic()).unwrap();
        init_db_and_create_account(
            db_path_str,
            WalletNetwork::Main,
            &seed,
            Some(2_400_000),
            "Account 1",
        )
        .unwrap();
        crate::sync::update_chain_tip(db_path_str, WalletNetwork::Main, 2_500_000).unwrap();

        let birthday = scan_min_birthday(db_path_str);
        let straddle_start = birthday - 50_000;
        let straddle_end = birthday + 50_000; // B + x, x = 50_000

        // State S14: a single FoundNote (priority 40) range straddling the
        // birthday and NO leftover Scanned range. (A note straddling B could be
        // queued by OpenAdjacent/FoundNote propagation or a Verify lookahead
        // belonging to the now-deleted old-birthday account.)
        {
            let conn = rusqlite::Connection::open(db_path_str).unwrap();
            conn.execute("DELETE FROM scan_queue", []).unwrap();
            conn.execute(
                "INSERT INTO scan_queue (block_range_start, block_range_end, priority) \
                 VALUES (:s, :e, 40)",
                named_params![":s": straddle_start, ":e": straddle_end],
            )
            .unwrap();
        }

        let demoted = prune_orphaned_scan_ranges(db_path_str).unwrap();
        assert_eq!(demoted, 1, "the straddler split counts exactly once");

        let conn = rusqlite::Connection::open(db_path_str).unwrap();
        // EXACT post-prune scan_queue: [straddle_start, B) pri=0, [B, B+x) pri=40.
        let rows: Vec<(i64, i64, i64)> = conn
            .prepare(
                "SELECT block_range_start, block_range_end, priority \
                 FROM scan_queue ORDER BY block_range_start ASC",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            rows,
            vec![
                (straddle_start, birthday, 0), // below-B remainder -> Ignored
                (birthday, straddle_end, 40),  // at/above-B portion -> FoundNote kept
            ],
            "split must demote only the below-birthday remainder and preserve \
             the high-priority FoundNote coverage at/above the birthday",
        );

        // The above-birthday FoundNote portion must NOT have been blanket-demoted:
        // it is the survivor's real note-discovery work and must remain pending.
        assert!(
            pending_scan_coverage_at_or_above(db_path_str, birthday),
            "the FoundNote portion above the birthday must survive as pending",
        );
        // No pending coverage below the birthday (the deleted account's hint is gone).
        assert!(!pending_scan_coverage_below(db_path_str, birthday));
        // No Scanned range below the birthday => block_fully_scanned will not be
        // pinned to a stale low height (it returns None here, summary falls back
        // to birthday-1, and the FoundNote range remains as legitimate work).
        match lowest_scanned_range_start(db_path_str) {
            None => {}
            Some(start) => assert!(start >= birthday),
        }

        // Idempotent: a second pass changes nothing.
        assert_eq!(prune_orphaned_scan_ranges(db_path_str).unwrap(), 0);
    }

    /// State S7 — the exact real-device VZR-89 shape: a deleted lower-birthday
    /// (Imported) account that synced PAST the surviving (higher) birthday before
    /// deletion leaves a `Scanned` range that STRADDLES the risen birthday. The
    /// split must keep `[B, end)` as `Scanned` (real blocks back it) and Ignore
    /// only `[start, B)`, so the lowest `Scanned` range starts at/above the
    /// birthday and `block_fully_scanned` is not pinned below it. Distinct from
    /// the FoundNote straddler test: here the kept upper half is `Scanned` — the
    /// exact priority `block_fully_scanned` keys off.
    #[test]
    fn test_prune_splits_scanned_straddler_keeps_upper_half_scanned() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("wallet.db");
        let db_path_str = db_path.to_str().unwrap();

        let seed = mnemonic_to_seed(&generate_mnemonic()).unwrap();
        init_db_and_create_account(
            db_path_str,
            WalletNetwork::Main,
            &seed,
            Some(2_400_000),
            "Account 1",
        )
        .unwrap();
        crate::sync::update_chain_tip(db_path_str, WalletNetwork::Main, 2_500_000).unwrap();

        let birthday = scan_min_birthday(db_path_str);
        let straddle_start = birthday - 1_000_000; // below B
        let straddle_end = birthday + 30_000; // above B

        {
            let conn = rusqlite::Connection::open(db_path_str).unwrap();
            conn.execute("DELETE FROM scan_queue", []).unwrap();
            // Scanned range straddling the risen birthday.
            conn.execute(
                "INSERT INTO scan_queue (block_range_start, block_range_end, priority) \
                 VALUES (:s, :e, 10)",
                named_params![":s": straddle_start, ":e": straddle_end],
            )
            .unwrap();
            // Pending remainder up to the tip (legitimate work above the birthday).
            conn.execute(
                "INSERT INTO scan_queue (block_range_start, block_range_end, priority) \
                 VALUES (:s, 2500001, 20)",
                named_params![":s": straddle_end],
            )
            .unwrap();
        }

        // Before the fix the lowest Scanned range started below the birthday.
        assert_eq!(
            lowest_scanned_range_start(db_path_str),
            Some(straddle_start)
        );

        let demoted = prune_orphaned_scan_ranges(db_path_str).unwrap();
        assert!(demoted >= 1);

        let conn = rusqlite::Connection::open(db_path_str).unwrap();
        // [B, straddle_end) stays Scanned ...
        let upper_scanned: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM scan_queue \
                 WHERE block_range_start = :b AND block_range_end = :e AND priority = 10)",
                named_params![":b": birthday, ":e": straddle_end],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            upper_scanned,
            "the split must keep [birthday, end) as Scanned"
        );
        // ... and [start, B) became Ignored.
        let lower_ignored: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM scan_queue \
                 WHERE block_range_start = :s AND block_range_end = :b AND priority = 0)",
                named_params![":s": straddle_start, ":b": birthday],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            lower_ignored,
            "the below-birthday half of the Scanned straddler must be Ignored",
        );
        drop(conn);

        // KEY: no Scanned range remains below the birthday, so block_fully_scanned
        // keys off [B, end) rather than a stale low height.
        assert!(lowest_scanned_range_start(db_path_str).unwrap() >= birthday);
        assert!(!pending_scan_coverage_below(db_path_str, birthday));
        assert!(pending_scan_coverage_at_or_above(db_path_str, birthday));

        // Idempotent.
        assert_eq!(prune_orphaned_scan_ranges(db_path_str).unwrap(), 0);
    }

    /// Codex follow-up: proves the rescue prune must run AFTER `update_chain_tip`.
    /// When the blocks table's `max_scanned` sits BELOW the surviving birthday
    /// (a deleted account that synced only a low region left a scanned block
    /// there), librustzcash's `update_chain_tip` anchors new ranges at
    /// `max_scanned + 1` WITHOUT clamping to the birthday, so it re-creates
    /// sub-birthday pending work. A prune that ran only before `update_chain_tip`
    /// would miss it; the prune run afterwards demotes it.
    #[test]
    fn test_prune_cleans_subbirthday_range_recreated_by_update_chain_tip() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("wallet.db");
        let db_path_str = db_path.to_str().unwrap();

        let seed = mnemonic_to_seed(&generate_mnemonic()).unwrap();
        init_db_and_create_account(
            db_path_str,
            WalletNetwork::Main,
            &seed,
            Some(2_400_000),
            "Account 1",
        )
        .unwrap();
        crate::sync::update_chain_tip(db_path_str, WalletNetwork::Main, 2_500_000).unwrap();
        let birthday = scan_min_birthday(db_path_str);

        // Simulate a deleted account's leftover scanned block BELOW the birthday:
        // a single blocks-table row at 746399 makes max_scanned < birthday.
        {
            let conn = rusqlite::Connection::open(db_path_str).unwrap();
            conn.execute(
                "INSERT INTO blocks (height, hash, time, sapling_tree) \
                 VALUES (746399, X'00', 0, X'')",
                [],
            )
            .unwrap();
        }

        // Re-run update_chain_tip (as the sync does). With max_scanned (746399)
        // below the birthday, it re-creates sub-birthday pending work anchored at
        // max_scanned + 1, NOT clamped to the birthday.
        crate::sync::update_chain_tip(db_path_str, WalletNetwork::Main, 2_500_000).unwrap();
        assert!(
            pending_scan_coverage_below(db_path_str, birthday),
            "update_chain_tip should re-create sub-birthday pending work from max_scanned+1",
        );

        // The rescue prune, run AFTER update_chain_tip, must demote it.
        prune_orphaned_scan_ranges(db_path_str).unwrap();
        assert!(
            !pending_scan_coverage_below(db_path_str, birthday),
            "prune after update_chain_tip must demote the re-created sub-birthday range",
        );
        assert!(pending_scan_coverage_at_or_above(db_path_str, birthday));
    }

    #[test]
    fn test_delete_account_handles_internal_sent_note_to_deleted_account() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("wallet.db");
        let db_path_str = db_path.to_str().unwrap();

        let first_phrase = generate_mnemonic();
        let first_seed = mnemonic_to_seed(&first_phrase).unwrap();
        let first_uuid = init_db_and_create_account(
            db_path_str,
            WalletNetwork::Main,
            &first_seed,
            None,
            "Account 1",
        )
        .unwrap()
        .uuid;

        let second_uuid = import_derived_account_at_index(
            db_path_str,
            WalletNetwork::Main,
            &first_seed,
            None,
            1,
            "Account 2",
        )
        .unwrap()
        .uuid;

        seed_internal_sent_note_to_account(db_path_str, &first_uuid, &second_uuid);

        delete_account(db_path_str, WalletNetwork::Main, &second_uuid).unwrap();

        let accounts = list_accounts(db_path_str, WalletNetwork::Main).unwrap();
        assert_eq!(accounts.len(), 1);
        assert!(accounts.iter().all(|account| account.name != "second"));
        assert_internal_sent_note_rewritten(db_path_str);
    }

    #[test]
    fn network_in_wallet_db_reads_the_network_from_an_account() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("wallet.db");
        let db_path_str = db_path.to_str().unwrap();
        assert_eq!(network_in_wallet_db(db_path_str).unwrap(), None);

        let seed = mnemonic_to_seed(&generate_mnemonic()).unwrap();
        let uuid =
            init_db_and_create_account(db_path_str, WalletNetwork::Test, &seed, None, "Account 1")
                .unwrap()
                .uuid;
        assert_eq!(
            network_in_wallet_db(db_path_str).unwrap(),
            Some(WalletNetwork::Test)
        );

        delete_account(db_path_str, WalletNetwork::Test, &uuid).unwrap();
        assert_eq!(network_in_wallet_db(db_path_str).unwrap(), None);
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

    fn seed_internal_sent_note_to_account(db_path: &str, from_uuid: &str, to_uuid: &str) {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute("PRAGMA foreign_keys = ON", []).unwrap();

        let from_account_id = account_row_id(&conn, from_uuid);
        let to_account_id = account_row_id(&conn, to_uuid);
        let funding_txid = vec![0xCD_u8; 32];
        conn.execute(
            "INSERT INTO transactions (txid, mined_height, min_observed_height)
             VALUES (?1, ?2, ?2)",
            rusqlite::params![funding_txid, 9_i64],
        )
        .unwrap();
        let funding_transaction_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO sapling_received_notes (
                 transaction_id, output_index, account_id, diversifier, value,
                 rcm, is_change
             ) VALUES (?1, 0, ?2, x'01', 2000, x'01', 0)",
            rusqlite::params![funding_transaction_id, from_account_id],
        )
        .unwrap();
        let from_received_note_id = conn.last_insert_rowid();

        let txid = vec![0xAB_u8; 32];
        conn.execute(
            "INSERT INTO transactions (txid, mined_height, min_observed_height)
             VALUES (?1, ?2, ?2)",
            rusqlite::params![txid, 10_i64],
        )
        .unwrap();
        let transaction_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO sapling_received_note_spends (
                 sapling_received_note_id, transaction_id
             ) VALUES (?1, ?2)",
            rusqlite::params![from_received_note_id, transaction_id],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO addresses (account_id, key_scope, address, receiver_flags)
             VALUES (?1, -1, ?2, 0)",
            rusqlite::params![to_account_id, "u1internalrecipient"],
        )
        .unwrap();
        let address_id = conn.last_insert_rowid();

        conn.execute(
            "INSERT INTO sapling_received_notes (
                 transaction_id, output_index, account_id, diversifier, value,
                 rcm, is_change, address_id
             ) VALUES (?1, 0, ?2, x'00', 1000, x'00', 0, ?3)",
            rusqlite::params![transaction_id, to_account_id, address_id],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO sent_notes (
                 transaction_id, output_pool, output_index, from_account_id,
                 to_account_id, value
             ) VALUES (?1, 2, 0, ?2, ?3, 1000)",
            rusqlite::params![transaction_id, from_account_id, to_account_id],
        )
        .unwrap();
    }

    fn assert_internal_sent_note_rewritten(db_path: &str) {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        let sent_note_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sent_notes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(sent_note_count, 1);

        let (to_address, to_account_id): (Option<String>, Option<i64>) = conn
            .query_row(
                "SELECT to_address, to_account_id FROM sent_notes",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(to_address.as_deref(), Some("u1internalrecipient"));
        assert_eq!(to_account_id, None);
    }

    fn account_row_id(conn: &rusqlite::Connection, account_uuid: &str) -> i64 {
        let uuid = uuid::Uuid::parse_str(account_uuid).unwrap();
        conn.query_row(
            "SELECT id FROM accounts WHERE uuid = ?1",
            rusqlite::params![uuid.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .unwrap()
    }
}
