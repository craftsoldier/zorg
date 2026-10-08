use std::{
    collections::HashSet,
    path::Path,
    sync::{Mutex, OnceLock},
};

use rusqlite::{named_params, OptionalExtension};
use secrecy::{ExposeSecret, SecretVec};

use zcash_client_backend::data_api::{
    chain::ChainState, Account as _, AccountBirthday, WalletRead, WalletWrite,
};
use zcash_client_sqlite::{wallet::init::init_wallet_db, AccountUuid};
use zcash_keys::keys::{UnifiedFullViewingKey, UnifiedSpendingKey};
use zcash_primitives::block::BlockHash;
use zcash_protocol::consensus::{BlockHeight, NetworkUpgrade, Parameters};

use crate::{
    db::{
        open_readonly_conn_with_timeout, open_wallet_db_for_read_with_timeout,
        open_wallet_db_with_timeout, with_wallet_db_write_lock, WalletDatabase,
        ACCOUNT_MUTATION_DB_BUSY_TIMEOUT, READ_DB_BUSY_TIMEOUT, WALLET_DB_BUSY_TIMEOUT,
    },
    network::WalletNetwork,
};

use crate::keys::*;

// ======================== Public API Structs ========================

/// A newly created account.
pub struct CreatedAccount {
    pub uuid: String,
    pub number: u32,
    pub unified_address: String,
}

#[derive(Debug)]
pub struct AccountInfo {
    pub name: String,
    pub unified_address: String,
    pub account_index: u32,   // shown to users as index + 1
    pub birthday_height: u32, // scan-from height
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

fn zip32_account_id(account_index: u32) -> Result<zip32::AccountId, String> {
    zip32::AccountId::try_from(account_index)
        .map_err(|_| format!("Invalid ZIP32 account index: {account_index}"))
}

fn unified_spending_key_for_account(
    network: WalletNetwork,
    seed: &SecretVec<u8>,
    account_index: u32,
) -> Result<UnifiedSpendingKey, String> {
    let account_id = zip32_account_id(account_index)?;
    UnifiedSpendingKey::from_seed(&network, seed.expose_secret(), account_id)
        .map_err(|e| format!("USK derivation failed for account {account_index}: {e:?}"))
}

fn software_account_ufvk(
    network: WalletNetwork,
    seed: &SecretVec<u8>,
    account_index: u32,
) -> Result<UnifiedFullViewingKey, String> {
    Ok(
        unified_spending_key_for_account(network, seed, account_index)?
            .to_unified_full_viewing_key(),
    )
}

/// Init DB + create the bootstrap software account as Derived.
/// This pins the DB seed fingerprint for seed-aware initialization, but the
/// account may later be deleted like any other non-final account.
/// Returns (account_uuid, unified_address).
/// Accounts are always named after their index: index 0 is "Account 1".
fn account_name_for_index(account_index: u32) -> String {
    format!("Account {}", account_index + 1)
}

/// Current receive address for a numbered account. `None` = the only account.
pub fn account_address_by_number(
    db_path: &str,
    network: WalletNetwork,
    number: Option<u32>,
) -> Result<String, String> {
    let accounts = list_accounts(db_path, network)?;
    let picked = pick_account(&accounts, number)?;
    let uuid = uuid_for_index(db_path, network, picked.account_index)?;

    let db = open_wallet_db_for_read(db_path, network)?;
    let account_id = parse_account_uuid(&uuid)?;
    let account = db
        .get_account(account_id)
        .map_err(|e| format!("Failed to get account: {e}"))?
        .ok_or("Account not found")?;
    let ufvk = account.ufvk().ok_or("Account does not have a UFVK")?;
    current_receive_address(&db, network, account_id, ufvk)
}

/// The next derived account index is MAX(hd_account_index) + 1, or 0 when
/// the wallet has no derived accounts yet. (One wallet file binds one seed,
/// so no per-seed scoping is needed.)
fn next_derived_index(db_path: &str) -> Result<u32, String> {
    let conn = open_readonly_conn_with_timeout(db_path, Some(READ_DB_BUSY_TIMEOUT))?;
    let next: u32 = conn
        .query_row(
            "SELECT COALESCE(MAX(hd_account_index) + 1, 0) FROM accounts \
             WHERE hd_account_index IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .map_err(|e| format!("Failed to read account index: {e}"))?;
    Ok(next)
}

/// Init DB + create the bootstrap software account as Derived.
/// This pins the DB seed fingerprint for seed-aware initialization, but the
/// account may later be deleted like any other non-final account.
/// The account is named after its index ("Account N"); the bootstrap account
/// is index 0, i.e. "Account 1".
pub fn init_db_and_create_account(
    db_path: &str,
    network: WalletNetwork,
    seed: &SecretVec<u8>,
    birthday_height: Option<u64>,
) -> Result<CreatedAccount, String> {
    if let Err(e) = ensure_db_initialized_with_seed(db_path, network, seed) {
        if wallet_exists(db_path) {
            return Err(format!(
                "A wallet already exists at {db_path} bound to a different seed. \
                 One wallet file holds one seed; use `--db <path>` to open a separate wallet. \
                 (Underlying error: {e})"
            ));
        }
        return Err(e);
    }

    let birthday = make_birthday(network, birthday_height);

    let (account_id, usk, number) = with_wallet_db_write_lock("keys.create_account", || {
        let mut db = open_wallet_db_for_mutation(db_path, network)?;

        // The backend derives the new account at MAX(hd_account_index)+1 for
        // this seed; compute it up front so the name and reported number match.
        let next_index = next_derived_index(db_path)?;

        // The bootstrap account uses create_account (Derived) so initial
        // seed-aware DB setup records the seed fingerprint.
        let (account_id, usk) = db
            .create_account(&account_name_for_index(next_index), seed, &birthday, None)
            .map_err(|e| format!("Failed to create account: {e}"))?;
        Ok::<_, String>((account_id, usk, next_index + 1))
    })?;

    let ufvk = usk.to_unified_full_viewing_key();
    let (ua, _di) = ufvk
        .default_address(shielded_address_request())
        .map_err(|e| format!("Failed to derive address: {e}"))?;

    Ok(CreatedAccount {
        uuid: account_id.expose_uuid().to_string(),
        number,
        unified_address: ua.encode(&network),
    })
}

/// Import a same-seed software account for a specific ZIP32 account index as a
/// derived account. This is used only after the first seed-anchor account has
/// initialized the wallet DB with the same seed.
/// The account is named after its index ("Account N").
pub fn import_derived_account_at_index(
    db_path: &str,
    network: WalletNetwork,
    seed: &SecretVec<u8>,
    birthday_height: Option<u64>,
    account_index: u32,
) -> Result<CreatedAccount, String> {
    let birthday = make_birthday(network, birthday_height);
    let account_id = zip32_account_id(account_index)?;
    let ufvk = software_account_ufvk(network, seed, account_index)?;
    let (ua, _di) = ufvk
        .default_address(shielded_address_request())
        .map_err(|e| format!("Failed to derive address: {e}"))?;

    let account = with_wallet_db_write_lock("keys.import_derived_account_at_index", || {
        let mut db = open_wallet_db_for_mutation(db_path, network)?;
        db.import_account_hd(
            &account_name_for_index(account_index),
            seed,
            account_id,
            &birthday,
            None,
        )
        .map(|(account, _usk)| account)
        .map_err(|e| format!("Failed to import derived account: {e}"))
    })?;

    Ok(CreatedAccount {
        uuid: account.id().expose_uuid().to_string(),
        number: account_index + 1,
        unified_address: ua.encode(&network),
    })
}

/// Resolve an account number (1-based, as shown by `zorg accounts`) to the
/// account's backend uuid. `None` means "the only account" and errors when
/// ambiguous.
pub fn resolve_account_uuid(
    db_path: &str,
    network: WalletNetwork,
    number: Option<u32>,
) -> Result<String, String> {
    let accounts = list_accounts(db_path, network)?;
    let picked = pick_account(&accounts, number)?;
    uuid_for_index(db_path, network, picked.account_index)
}

/// Internal: the backend uuid for a numbered account. Single-seed CLI wallets
/// keep at most a handful of accounts, so a linear scan is fine.
fn uuid_for_index(
    db_path: &str,
    network: WalletNetwork,
    account_index: u32,
) -> Result<String, String> {
    let db = open_wallet_db_for_read(db_path, network)?;
    for id in db
        .get_account_ids()
        .map_err(|e| format!("Failed to list accounts: {e}"))?
    {
        let account = db
            .get_account(id)
            .map_err(|e| format!("Failed to get account: {e}"))?
            .ok_or("Account not found")?;
        if let Some(d) = account.source().key_derivation() {
            if u32::from(d.account_index()) == account_index {
                return Ok(id.expose_uuid().to_string());
            }
        }
    }
    Err(format!("No account with index {account_index}"))
}

fn pick_account(accounts: &[AccountInfo], number: Option<u32>) -> Result<&AccountInfo, String> {
    let n = match number {
        None => match accounts.len() {
            0 => return Err("No accounts found. Run `zorg create`.".into()),
            1 => 1,
            _ => {
                return Err(format!(
                    "Multiple accounts; pass --account <number>. `zorg accounts` lists {}.",
                    accounts.len()
                ))
            }
        },
        Some(0) => return Err("Accounts are numbered from 1.".into()),
        Some(n) => n,
    };
    match accounts.iter().find(|a| a.account_index + 1 == n as u32) {
        Some(account) => Ok(account),
        None => {
            let valid: Vec<String> = accounts
                .iter()
                .map(|a| (a.account_index + 1).to_string())
                .collect();
            Err(format!(
                "No account #{n}; valid numbers: {}",
                valid.join(", ")
            ))
        }
    }
}

/// List all numbered (seed-derived) accounts in the wallet database.
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
            name: account.name().unwrap_or("").to_string(),
            unified_address: address,
            account_index: u32::from(derivation.account_index()),
            birthday_height: u32::from(account.birthday_height()),
        });
    }

    accounts.sort_by_key(|a| a.account_index);
    Ok(accounts)
}

pub fn list_account_uuids_from_db(db_path: &str) -> Result<Vec<String>, String> {
    let conn = open_readonly_conn_with_timeout(db_path, Some(READ_DB_BUSY_TIMEOUT))?;
    let mut stmt = conn
        .prepare("SELECT uuid FROM accounts ORDER BY id ASC")
        .map_err(|e| format!("Failed to prepare account UUID query: {e}"))?;
    let rows = stmt
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .map_err(|e| format!("Failed to list account UUIDs: {e}"))?;

    let mut uuids = Vec::new();
    for row in rows {
        let bytes = row.map_err(|e| format!("Failed to read account UUID: {e}"))?;
        let uuid = uuid::Uuid::from_slice(&bytes)
            .map_err(|e| format!("Invalid account UUID bytes in DB: {e}"))?;
        uuids.push(uuid.to_string());
    }
    Ok(uuids)
}

/// Delete the account with the given number.
/// Resolves fresh at call time, so it always targets the account the number
/// currently points at.
pub fn delete_account_by_number(
    db_path: &str,
    network: WalletNetwork,
    number: u32,
) -> Result<(), String> {
    let uuid = resolve_account_uuid(db_path, network, Some(number))?;
    delete_account(db_path, network, &uuid)
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

/// Returns the standard shielded address request (Orchard + Sapling, no transparent).
/// This matches the behavior of zodl/Zashi wallets.
pub fn wallet_exists(db_path: &str) -> bool {
    Path::new(db_path).exists()
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
            init_db_and_create_account(db_path_str, WalletNetwork::Main, &seed, None).unwrap();
        let address = created.unified_address;

        // Mainnet unified addresses start with "u1"
        assert!(
            address.starts_with("u1"),
            "Expected u1 prefix, got: {address}"
        );

        // Verify we can read the address back
        let address2 =
            account_address_by_number(db_path_str, WalletNetwork::Main, Some(1)).unwrap();
        assert_eq!(address, address2);
    }

    #[test]
    fn derived_accounts_are_numbered_and_sorted() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path_str = temp_dir.path().join("wallet.db");
        let db_path_str = db_path_str.to_str().unwrap();

        let phrase = generate_mnemonic();
        let seed = mnemonic_to_seed(&phrase).unwrap();
        let first =
            init_db_and_create_account(db_path_str, WalletNetwork::Main, &seed, None).unwrap();
        assert_eq!(first.number, 1);

        // Same seed again: the backend derives the next ZIP-32 account.
        let second =
            init_db_and_create_account(db_path_str, WalletNetwork::Main, &seed, None).unwrap();
        assert_eq!(second.number, 2);
        assert_ne!(first.uuid, second.uuid);

        let accounts = list_accounts(db_path_str, WalletNetwork::Main).unwrap();
        assert_eq!(accounts.len(), 2);
        assert_eq!(accounts[0].name, "Account 1");
        assert_eq!(accounts[0].account_index, 0);
        assert_eq!(accounts[1].name, "Account 2");
        assert_eq!(accounts[1].account_index, 1);
    }

    #[test]
    fn pick_account_is_strictly_numbers() {
        let mk = |n: u32, name: &str| AccountInfo {
            name: name.to_string(),
            unified_address: String::new(),
            account_index: n - 1,
            birthday_height: 1,
        };
        let accounts = vec![mk(1, "Account 1"), mk(2, "Account 2")];

        // numbers select positionally in the sorted listing
        assert_eq!(pick_account(&accounts, Some(2)).unwrap().name, "Account 2");
        // a bare reference resolves when the wallet has exactly one account
        assert_eq!(
            pick_account(&accounts[..1], None).unwrap().name,
            "Account 1"
        );
        // strictly numbers: anything that is not a number is rejected by the
        // UI-boundary parser before pick_account ever sees it
        assert!(pick_account(&accounts, Some(9))
            .unwrap_err()
            .contains("No account #9"));
        assert!(pick_account(&accounts, None)
            .unwrap_err()
            .contains("--account"));
        assert!(pick_account(&[], None).unwrap_err().contains("zorg create"));
    }

    #[test]
    fn test_get_address_returns_last_generated_receive_address() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("wallet.db");
        let db_path_str = db_path.to_str().unwrap();

        let phrase = generate_mnemonic();
        let seed = mnemonic_to_seed(&phrase).unwrap();

        let created =
            init_db_and_create_account(db_path_str, WalletNetwork::Main, &seed, None).unwrap();
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
        init_db_and_create_account(db_path_str, WalletNetwork::Main, &first_seed, None).unwrap();

        // Second numbered account, derived from the same seed at index 1.
        let second_uuid =
            import_derived_account_at_index(db_path_str, WalletNetwork::Main, &first_seed, None, 1)
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
        )
        .unwrap();
        crate::sync::update_chain_tip(db_path_str, WalletNetwork::Main, 2_500_000).unwrap();

        // Capture the surviving account's birthday BEFORE the import: this is
        // the threshold the orphan must end up below. (MIN(birthday) read after
        // the import would include the imported account's old birthday.)
        let surviving_birthday = scan_min_birthday(db_path_str);

        let imported_seed = mnemonic_to_seed(&generate_mnemonic()).unwrap();
        let imported_uuid = import_derived_account_at_index(
            db_path_str,
            WalletNetwork::Main,
            &imported_seed,
            Some(419_200),
            1,
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
        init_db_and_create_account(db_path_str, WalletNetwork::Main, &seed, Some(2_400_000))
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
        init_db_and_create_account(db_path_str, WalletNetwork::Main, &seed, Some(2_400_000))
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
        init_db_and_create_account(db_path_str, WalletNetwork::Main, &seed, Some(2_400_000))
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
        init_db_and_create_account(db_path_str, WalletNetwork::Main, &seed, Some(2_400_000))
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
        init_db_and_create_account(db_path_str, WalletNetwork::Main, &seed, Some(2_400_000))
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
        init_db_and_create_account(db_path_str, WalletNetwork::Main, &seed, Some(2_400_000))
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
        let first_uuid =
            init_db_and_create_account(db_path_str, WalletNetwork::Main, &first_seed, None)
                .unwrap()
                .uuid;

        let second_phrase = generate_mnemonic();
        let second_seed = mnemonic_to_seed(&second_phrase).unwrap();
        let second_uuid = import_derived_account_at_index(
            db_path_str,
            WalletNetwork::Main,
            &second_seed,
            None,
            1,
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
    fn test_delete_account_allows_seed_anchor_with_remaining_accounts() {
        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("wallet.db");
        let db_path_str = db_path.to_str().unwrap();

        let first_phrase = generate_mnemonic();
        let first_seed = mnemonic_to_seed(&first_phrase).unwrap();
        let first_uuid =
            init_db_and_create_account(db_path_str, WalletNetwork::Main, &first_seed, None)
                .unwrap()
                .uuid;

        // A second numbered account derived from the same seed at index 1.
        import_derived_account_at_index(db_path_str, WalletNetwork::Main, &first_seed, None, 1)
            .unwrap();

        delete_account(db_path_str, WalletNetwork::Main, &first_uuid).unwrap();

        let accounts = list_accounts(db_path_str, WalletNetwork::Main).unwrap();
        assert_eq!(accounts.len(), 1);
        assert!(accounts.iter().all(|account| account.name != "first"));
        assert_eq!(accounts[0].account_index, 1);
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
