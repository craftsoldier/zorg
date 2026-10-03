use zcash_client_backend::{
    data_api::{
        chain::{scan_cached_blocks, CommitmentTreeRoot},
        scanning::ScanPriority,
        WalletCommitmentTrees, WalletRead, WalletWrite,
    },
    proto::service::TreeState,
};
use zcash_client_sqlite::{
    chain::{init::init_blockmeta_db, BlockMeta},
    AccountUuid, FsBlockDb,
};
use zcash_primitives::block::BlockHash;
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
    parse_address_request_kind, set_transaction_status, AddressRequestKind, WalletBalance,
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

fn open_block_cache(cache_path: &str) -> Result<FsBlockDb, String> {
    std::fs::create_dir_all(cache_path).map_err(|e| format!("Failed to create cache dir: {e}"))?;
    let mut db_cache = FsBlockDb::for_path(cache_path)
        .map_err(|e| format!("Failed to open block cache: {e:?}"))?;
    init_blockmeta_db(&mut db_cache).map_err(|e| format!("Failed to init block cache: {e}"))?;
    Ok(db_cache)
}

// ======================== Sync ========================

pub fn update_chain_tip(db_path: &str, network: WalletNetwork, height: u64) -> Result<(), String> {
    with_wallet_db_write_lock("sync.update_chain_tip", || {
        let mut db = open_wallet_db(db_path, network)?;
        db.update_chain_tip(BlockHeight::from_u32(height as u32))
            .map_err(|e| format!("Failed to update chain tip: {e}"))
    })
}

/// Get next subtree indices to know where to start downloading from.
pub fn get_next_subtree_indices(
    db_path: &str,
    network: WalletNetwork,
) -> Result<(u64, u64, u64), String> {
    let summary = crate::wallet_summary_cache::get_wallet_summary_cached(db_path, network)?;
    match summary {
        Some(s) => Ok((
            s.next_sapling_subtree_index(),
            s.next_orchard_subtree_index(),
            s.next_ironwood_subtree_index(),
        )),
        None => Ok((0, 0, 0)),
    }
}

pub fn put_sapling_subtree_roots(
    db_path: &str,
    network: WalletNetwork,
    start_index: u64,
    roots: &[(u64, Vec<u8>)],
) -> Result<(), String> {
    let parsed: Vec<_> = roots
        .iter()
        .map(|(h, bytes)| {
            let arr: [u8; 32] = bytes.as_slice().try_into().map_err(|_| "bad hash len")?;
            let node =
                Option::from(sapling_crypto::Node::from_bytes(arr)).ok_or("bad sapling hash")?;
            Ok::<_, String>(CommitmentTreeRoot::from_parts(
                BlockHeight::from_u32(*h as u32),
                node,
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if parsed.is_empty() {
        Ok(())
    } else {
        with_wallet_db_write_lock("sync.put_sapling_subtree_roots", || {
            let mut db = open_wallet_db(db_path, network)?;
            db.put_sapling_subtree_roots(start_index, parsed.as_slice())
                .map_err(|e| format!("{e}"))
        })
    }
}

pub fn put_orchard_subtree_roots(
    db_path: &str,
    network: WalletNetwork,
    start_index: u64,
    roots: &[(u64, Vec<u8>)],
) -> Result<(), String> {
    let parsed: Vec<_> = roots
        .iter()
        .map(|(h, bytes)| {
            let arr: [u8; 32] = bytes.as_slice().try_into().map_err(|_| "bad hash len")?;
            let node = Option::from(orchard::tree::MerkleHashOrchard::from_bytes(&arr))
                .ok_or("bad orchard hash")?;
            Ok::<_, String>(CommitmentTreeRoot::from_parts(
                BlockHeight::from_u32(*h as u32),
                node,
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if parsed.is_empty() {
        Ok(())
    } else {
        with_wallet_db_write_lock("sync.put_orchard_subtree_roots", || {
            let mut db = open_wallet_db(db_path, network)?;
            db.put_orchard_subtree_roots(start_index, parsed.as_slice())
                .map_err(|e| format!("{e}"))
        })
    }
}

pub fn put_ironwood_subtree_roots(
    db_path: &str,
    network: WalletNetwork,
    start_index: u64,
    roots: &[(u64, Vec<u8>)],
) -> Result<(), String> {
    let parsed: Vec<_> = roots
        .iter()
        .map(|(h, bytes)| {
            let arr: [u8; 32] = bytes.as_slice().try_into().map_err(|_| "bad hash len")?;
            let node = Option::from(orchard::tree::MerkleHashOrchard::from_bytes(&arr))
                .ok_or("bad ironwood hash")?;
            Ok::<_, String>(CommitmentTreeRoot::from_parts(
                BlockHeight::from_u32(*h as u32),
                node,
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if parsed.is_empty() {
        Ok(())
    } else {
        with_wallet_db_write_lock("sync.put_ironwood_subtree_roots", || {
            let mut db = open_wallet_db(db_path, network)?;
            db.put_ironwood_subtree_roots(start_index, parsed.as_slice())
                .map_err(|e| format!("{e}"))
        })
    }
}

pub struct ScanRangeInfo {
    pub start: u64,
    pub end: u64,
    pub priority: u8,
}

pub fn suggest_scan_ranges(
    db_path: &str,
    network: WalletNetwork,
) -> Result<Vec<ScanRangeInfo>, String> {
    let db = open_wallet_db_for_read(db_path, network)?;
    let ranges = db.suggest_scan_ranges().map_err(|e| format!("{e}"))?;
    Ok(ranges
        .into_iter()
        .filter(|r| r.priority() != ScanPriority::Ignored && r.priority() != ScanPriority::Scanned)
        .map(|r| ScanRangeInfo {
            start: u32::from(r.block_range().start) as u64,
            end: u32::from(r.block_range().end) as u64,
            priority: match r.priority() {
                ScanPriority::Verify => 6,
                ScanPriority::ChainTip => 5,
                ScanPriority::FoundNote => 4,
                ScanPriority::OpenAdjacent => 3,
                ScanPriority::Historic => 2,
                ScanPriority::Scanned => 1,
                ScanPriority::Ignored => 0,
            },
        })
        .collect())
}

pub fn write_block_metadata(
    cache_path: &str,
    blocks: &[(u64, Vec<u8>, u32, u32, u32)],
) -> Result<(), String> {
    let db_cache = open_block_cache(cache_path)?;
    let metas: Vec<BlockMeta> = blocks
        .iter()
        .map(|(h, hash, time, sc, oc)| {
            let mut arr = [0u8; 32];
            arr[..hash.len().min(32)].copy_from_slice(&hash[..hash.len().min(32)]);
            BlockMeta {
                height: BlockHeight::from_u32(*h as u32),
                block_hash: BlockHash(arr),
                block_time: *time,
                sapling_outputs_count: *sc,
                orchard_actions_count: *oc,
            }
        })
        .collect();
    db_cache
        .write_block_metadata(&metas)
        .map_err(|e| format!("{e:?}"))
}

#[allow(clippy::too_many_arguments)]
pub fn scan_blocks(
    db_path: &str,
    cache_path: &str,
    network: WalletNetwork,
    from_height: u64,
    ts_network: &str,
    ts_height: u64,
    ts_hash: &str,
    ts_time: u32,
    ts_sapling: &str,
    ts_orchard: &str,
    ts_ironwood: &str,
    limit: u64,
) -> Result<u64, String> {
    let db_cache = open_block_cache(cache_path)?;
    let from_state = if ts_hash.is_empty() {
        zcash_client_backend::data_api::chain::ChainState::empty(
            BlockHeight::from_u32((from_height - 1) as u32),
            BlockHash([0u8; 32]),
        )
    } else {
        TreeState {
            network: ts_network.into(),
            height: ts_height,
            hash: ts_hash.into(),
            time: ts_time,
            sapling_tree: ts_sapling.into(),
            orchard_tree: ts_orchard.into(),
            ironwood_tree: ts_ironwood.into(),
        }
        .to_chain_state()
        .map_err(|e| format!("{e}"))?
    };
    let result = with_wallet_db_write_lock("sync.scan_blocks", || {
        let mut db_data = open_wallet_db(db_path, network)?;
        scan_cached_blocks(
            &network,
            &db_cache,
            &mut db_data,
            BlockHeight::from_u32(from_height as u32),
            &from_state,
            limit as usize,
        )
        .map_err(|e| format!("{e}"))
    })?;
    Ok((u32::from(result.scanned_range().end) - u32::from(result.scanned_range().start)) as u64)
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

// ======================== Rewind ========================

pub fn rewind_to_height(db_path: &str, network: WalletNetwork, height: u64) -> Result<u64, String> {
    let result = with_wallet_db_write_lock("sync.rewind_to_height", || {
        let mut db = open_wallet_db(db_path, network)?;
        db.truncate_to_height(BlockHeight::from_u32(height as u32))
            .map_err(|e| format!("{e}"))
    })?;
    Ok(u32::from(result) as u64)
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

pub fn get_blocks_dir(cache_path: &str) -> String {
    format!("{cache_path}/blocks")
}

// Re-export aliases for API layer
pub(crate) use proposal_locks::recover_previous_process as recover_orphaned_send_locks;

// ======================== Re-exports from send.rs ========================
pub(crate) use send::resubmit_pending_transactions;
pub use send::{
    estimate_fee, execute_proposal, execute_proposal_with_seed_loader, propose_send,
    ExecuteProposalResult,
};

// ======================== Sync State & Orchestration ========================

use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;

pub static SYNC_CANCEL: std::sync::LazyLock<Arc<AtomicBool>> =
    std::sync::LazyLock::new(|| Arc::new(AtomicBool::new(false)));
pub static SYNC_RUNNING: AtomicBool = AtomicBool::new(false);

pub fn cancel_full_sync() {
    SYNC_CANCEL.store(true, AtomicOrdering::Relaxed);
}

pub fn is_sync_cancel_requested() -> bool {
    SYNC_CANCEL.load(AtomicOrdering::Relaxed)
}

pub fn is_sync_running() -> bool {
    SYNC_RUNNING.load(AtomicOrdering::SeqCst)
}

/// Blocking sync entrypoint.
pub fn run_full_sync_blocking(
    db_path: &str,
    lightwalletd_url: &str,
    network: &str,
) -> Result<(), String> {
    if SYNC_RUNNING
        .compare_exchange(false, true, AtomicOrdering::SeqCst, AtomicOrdering::SeqCst)
        .is_err()
    {
        return Err("Sync already running".into());
    }

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let network = crate::keys::parse_network(network)?;
        crate::account::ensure_db_migrated_once(db_path, network)?;
        let cancel = SYNC_CANCEL.clone();
        cancel.store(false, AtomicOrdering::Relaxed);

        let rt = tokio::runtime::Runtime::new().map_err(|e| format!("tokio: {e}"))?;
        rt.block_on(async {
            crate::sync_engine::run_sync_inner(
                db_path,
                lightwalletd_url,
                network,
                cancel,
                true,
                |_| {},
            )
            .await
        })
    }));

    SYNC_RUNNING.store(false, AtomicOrdering::SeqCst);

    match result {
        Ok(r) => r,
        Err(e) => {
            let msg = if let Some(s) = e.downcast_ref::<&str>() {
                s.to_string()
            } else if let Some(s) = e.downcast_ref::<String>() {
                s.clone()
            } else {
                "Unknown panic".to_string()
            };
            Err(format!("Sync panic: {msg}"))
        }
    }
}

// ======================== API-only Structs ========================

pub struct ScanResult {
    pub blocks_scanned: u64,
}

pub struct SubtreeRoot {
    pub completing_block_height: u64,
    pub root_hash: Vec<u8>,
}

pub struct BlockMetaInfo {
    pub height: u64,
    pub hash: Vec<u8>,
    pub time: u32,
    pub sapling_outputs_count: u32,
    pub orchard_actions_count: u32,
}

pub struct AddressValidationResult {
    pub is_valid: bool,
    pub address_type: String,
}

pub struct SubtreeIndices {
    pub sapling_start_index: u64,
    pub orchard_start_index: u64,
}
