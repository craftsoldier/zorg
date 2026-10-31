use zcash_client_backend::data_api::{WalletRead, WalletWrite};
use zcash_client_sqlite::AccountUuid;
use zcash_protocol::consensus::BlockHeight;

use crate::{
    db::{
        open_readonly_conn_with_timeout, open_wallet_db_for_read_with_timeout,
        open_wallet_db_with_timeout, with_wallet_db_write_lock, WalletDatabase,
        READ_DB_BUSY_TIMEOUT, WALLET_DB_BUSY_TIMEOUT,
    },
    network::WalletNetwork,
};

mod broadcast;
mod proposal_locks;
mod send;
mod transactions;

// Re-export the split submodules so every
// `crate::sync::propose_send` / `::get_wallet_balance` /
// etc. call paths keep resolving with
// the same visibility the monolithic `sync.rs` had before the refactor.
// Functions were `pub fn` in the old file → `pub use`. Return-value
// structs were `pub(crate) struct` → `pub(crate) use` (they're
// reachable from anywhere in the crate but not re-exported to
// downstream consumers, which matches the pre-refactor surface
// exactly).
pub(crate) use transactions::get_unmined_txids_with_mined_output_evidence;
pub use transactions::{
    decrypt_and_store_transaction, get_next_available_address,
    get_previous_transaction_count_for_address, get_transaction_data_requests,
    get_transaction_detail, get_transaction_history, get_wallet_balance, get_wallet_balances,
    parse_address_request_kind, AddressRequestKind, WalletBalance,
};

pub(super) fn open_wallet_db(
    db_path: &str,
    network: WalletNetwork,
) -> Result<WalletDatabase, String> {
    open_wallet_db_with_timeout(db_path, network, WALLET_DB_BUSY_TIMEOUT)
}

pub(crate) fn open_wallet_db_for_read(
    db_path: &str,
    network: WalletNetwork,
) -> Result<WalletDatabase, String> {
    open_wallet_db_for_read_with_timeout(db_path, network, READ_DB_BUSY_TIMEOUT)
}

pub(crate) fn open_readonly_conn(db_path: &str) -> Result<rusqlite::Connection, String> {
    open_readonly_conn_with_timeout(db_path, Some(READ_DB_BUSY_TIMEOUT))
}

// ======================== Sync ========================

pub fn update_chain_tip(db_path: &str, network: WalletNetwork, height: u64) -> Result<(), String> {
    with_wallet_db_write_lock("sync.update_chain_tip", || {
        let mut db = open_wallet_db(db_path, network)?;
        db.update_chain_tip(BlockHeight::from_u32(height as u32))
            .map_err(|e| format!("Failed to update chain tip: {e}"))
    })
}

// ======================== Balance & Progress ========================

pub struct SyncProgress {
    pub scanned_height: u64,
    pub chain_tip_height: u64,
    pub is_syncing: bool,
    pub is_complete: bool,
}

fn is_completed_sync_status(
    scanned_height: u64,
    chain_tip_height: u64,
    last_completed_height: Option<u64>,
) -> bool {
    chain_tip_height > 0
        && scanned_height >= chain_tip_height
        && last_completed_height == Some(chain_tip_height)
}

/// Reads only the two wallet heights needed for status and completion checks.
///
/// `WalletSummary` uses `birthday - 1` before the first block has been fully
/// scanned, so preserve that behavior when `block_fully_scanned` has no value.
pub(crate) fn wallet_scan_heights(db: &mut WalletDatabase) -> Result<Option<(u64, u64)>, String> {
    wallet_scan_heights_in_snapshot(db, || {})
}

fn wallet_scan_heights_in_snapshot(
    db: &mut WalletDatabase,
    after_chain_height: impl FnOnce(),
) -> Result<Option<(u64, u64)>, String> {
    // The callback is a deterministic test seam: the first SELECT has fixed
    // the SQLite snapshot before a concurrent writer is allowed to commit.
    // Production callers always pass a no-op.
    let mut after_chain_height = Some(after_chain_height);
    db.transactionally(|db| {
        let Some(chain_tip_height) = db.chain_height()? else {
            return Ok(None);
        };
        if let Some(after_chain_height) = after_chain_height.take() {
            after_chain_height();
        }
        let scanned_height = match db.block_fully_scanned()? {
            Some(block) => u32::from(block.block_height()) as u64,
            None => {
                let Some(birthday_height) = db.get_wallet_birthday()? else {
                    return Ok(None);
                };
                u32::from(birthday_height).saturating_sub(1) as u64
            }
        };

        Ok(Some((scanned_height, u32::from(chain_tip_height) as u64)))
    })
    .map_err(|e: zcash_client_sqlite::error::SqliteClientError| format!("{e}"))
}

pub fn get_sync_progress(db_path: &str, network: WalletNetwork) -> Result<SyncProgress, String> {
    let mut db = open_wallet_db_for_read(db_path, network)?;
    match wallet_scan_heights(&mut db)? {
        Some((scanned_height, chain_tip_height)) => {
            let last_completed_height = super::sync_engine::completed_sync_height_for_status(
                db_path,
                scanned_height,
                chain_tip_height,
            )
            .unwrap_or_else(|e| {
                log::warn!("sync: completed-height metadata unavailable: {e}");
                None
            });
            Ok(SyncProgress {
                scanned_height,
                chain_tip_height,
                is_syncing: scanned_height < chain_tip_height,
                is_complete: is_completed_sync_status(
                    scanned_height,
                    chain_tip_height,
                    last_completed_height,
                ),
            })
        }
        None => Ok(SyncProgress {
            scanned_height: 0,
            chain_tip_height: 0,
            is_syncing: false,
            is_complete: false,
        }),
    }
}

// ======================== Address Validation ========================

pub fn validate_address(address: &str) -> Result<String, String> {
    use zcash_address::ZcashAddress;
    use zcash_keys::address::Address;

    let addr: Address = ZcashAddress::try_from_encoded(address)
        .map_err(|e| format!("Invalid: {e}"))?
        .convert()
        .map_err(|e| format!("Invalid: {e}"))?;

    match addr {
        Address::Unified(_) => Ok("unified".into()),
        Address::Sapling(_) => Ok("sapling".into()),
        Address::Transparent(_) => Ok("transparent".into()),
        Address::Tex(_) => Ok("tex".into()),
    }
}

// ======================== Send ========================

/// Propose a transfer. Returns (proposal_id, needs_sapling_params, fee_zatoshi).
/// The proposal is stored internally and referenced by proposal_id for execute_proposal.
// In-memory proposal store (proposals are short-lived, between
// propose and execute). Kept in `sync/mod.rs` because it is shared
// between the software send flow (`send::execute_proposal`) and the
// software send flow (`send::execute_proposal`); placing
// it in either submodule would create a cross-submodule dependency.
use std::collections::HashMap;
use std::sync::Mutex;

pub(super) struct StoredProposal {
    pub proposal_id: u64,
    pub proposal: zcash_client_backend::proposal::Proposal<
        send::WalletFeeRule,
        zcash_client_sqlite::ReceivedNoteId,
    >,
    pub network: WalletNetwork,
    pub account_id: AccountUuid,
    pub send_flow_id: String,
}

#[derive(Clone)]
pub(super) struct StoredProposalLock {
    pub proposal: zcash_client_backend::proposal::Proposal<
        send::WalletFeeRule,
        zcash_client_sqlite::ReceivedNoteId,
    >,
    pub network: WalletNetwork,
    pub db_path: String,
    pub owner: zcash_client_backend::wallet::LockOwner,
    pub send_flow_id: String,
}

pub(super) static PROPOSAL_STORE: std::sync::LazyLock<Mutex<ProposalStore>> =
    std::sync::LazyLock::new(|| {
        Mutex::new(ProposalStore {
            proposals: HashMap::new(),
            locks: HashMap::new(),
            next_id: 1,
        })
    });

pub(super) struct ProposalStore {
    pub proposals: HashMap<u64, StoredProposal>,
    pub locks: HashMap<u64, StoredProposalLock>,
    pub next_id: u64,
}

pub(super) fn consume_stored_proposal(
    proposal_id: u64,
    send_flow_id: &str,
    not_found_message: &str,
) -> Result<StoredProposal, String> {
    let mut store = PROPOSAL_STORE
        .lock()
        .map_err(|e| format!("Lock error: {e}"))?;

    match store.proposals.get(&proposal_id) {
        Some(stored) if stored.send_flow_id == send_flow_id => {}
        Some(_) => {
            log::warn!("proposal store: send flow mismatch for proposal_id={proposal_id}");
            return Err("Send flow mismatch".to_string());
        }
        None => return Err(not_found_message.to_string()),
    }

    store
        .proposals
        .remove(&proposal_id)
        .ok_or_else(|| not_found_message.to_string())
}

pub(super) fn stored_proposal_lock(
    proposal_id: u64,
    send_flow_id: &str,
) -> Result<StoredProposalLock, String> {
    let store = PROPOSAL_STORE
        .lock()
        .map_err(|e| format!("Lock error: {e}"))?;
    match store.locks.get(&proposal_id) {
        Some(lock) if lock.send_flow_id == send_flow_id => Ok(lock.clone()),
        Some(_) => {
            log::warn!("proposal store: lock send flow mismatch for proposal_id={proposal_id}");
            Err("Send flow mismatch".to_string())
        }
        None => Err("Proposal input lock not found".to_string()),
    }
}

fn unlock_stored_proposal(
    proposal_id: u64,
    send_flow_id: &str,
    lock: StoredProposalLock,
) -> Result<(), String> {
    with_wallet_db_write_lock("sync.unlock_stored_proposal", || {
        let mut db = open_wallet_db(&lock.db_path, lock.network)?;
        // The wallet write lock is always acquired before the proposal-store
        // mutex (the same order used while creating proposals). Re-checking
        // here prevents a retain call that won the race from being followed by
        // a stale DB unlock.
        let mut store = PROPOSAL_STORE
            .lock()
            .map_err(|e| format!("Lock proposal store before DB unlock: {e}"))?;
        let current = match store.locks.get(&proposal_id) {
            Some(current) if current.send_flow_id == send_flow_id => current.clone(),
            Some(_) => return Err("Send flow mismatch before DB unlock".to_string()),
            None => return Ok(()),
        };
        zcash_client_backend::data_api::wallet::unlock_proposal_inputs(
            &mut db,
            &current.proposal,
            current.owner,
        )
        .map_err(|e| format!("Unlock abandoned send proposal inputs: {e}"))?;
        proposal_locks::remove(&current.db_path, current.owner)?;
        store.locks.remove(&proposal_id);
        Ok(())
    })
}

pub(super) fn finish_stored_proposal(
    proposal_id: u64,
    send_flow_id: &str,
    release_inputs: bool,
) -> Result<(), String> {
    let lock = {
        let mut store = PROPOSAL_STORE
            .lock()
            .map_err(|e| format!("Lock proposal store for finish: {e}"))?;
        let Some(lock) = store.locks.get(&proposal_id).cloned() else {
            return Ok(());
        };
        if lock.send_flow_id != send_flow_id {
            return Err("Send flow mismatch".to_string());
        }
        if !release_inputs {
            store.locks.remove(&proposal_id);
            drop(store);
            return proposal_locks::remove(&lock.db_path, lock.owner);
        }
        lock.clone()
    };

    // The DB helper re-checks ownership while holding both locks. On DB
    // failure the owner record remains in place, allowing an idempotent retry.
    unlock_stored_proposal(proposal_id, send_flow_id, lock)
}

// ======================== Helpers ========================

// Re-export aliases for API layer
pub(crate) use proposal_locks::recover_previous_process as recover_orphaned_send_locks;

// ======================== Re-exports from send.rs ========================
pub(crate) use send::resubmit_pending_transactions;
pub use send::{
    estimate_fee, execute_proposal_with_seed_loader, propose_send, ExecuteProposalResult,
};

// ======================== Sync State & Orchestration ========================

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

/// Blocking sync entrypoint.
pub fn run_full_sync_blocking(
    db_path: &str,
    lightwalletd_url: &str,
    network: &str,
) -> Result<(), String> {
    let network = crate::keys::parse_network(network)?;
    crate::account::ensure_db_migrated_once(db_path, network)?;
    let cancel = Arc::new(AtomicBool::new(false));

    let rt = tokio::runtime::Runtime::new().map_err(|e| format!("tokio: {e}"))?;
    rt.block_on(async {
        crate::sync_engine::run_sync_inner(db_path, lightwalletd_url, network, cancel, true, |_| {})
            .await
    })
}
