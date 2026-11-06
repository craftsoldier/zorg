use zcash_client_backend::data_api::WalletRead;

use crate::{
    db::{
        open_readonly_conn_with_timeout, open_wallet_db_for_read_with_timeout,
        open_wallet_db_with_timeout, WalletDatabase, READ_DB_BUSY_TIMEOUT, WALLET_DB_BUSY_TIMEOUT,
    },
    network::WalletNetwork,
};

mod broadcast;
mod proposal_locks;
mod send;
mod transactions;

pub(crate) use transactions::get_unmined_txids_with_mined_output_evidence;
pub use transactions::{
    get_next_available_address, get_transaction_detail, get_transaction_history,
    get_wallet_balance, WalletBalance,
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

// ======================== Balance & Progress ========================

pub struct SyncProgress {
    pub scanned_height: u64,
    pub chain_tip_height: u64,
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
            let last_completed_height =
                super::sync_engine::completed_sync_height_for_status(db_path).unwrap_or_else(|e| {
                    log::warn!("sync: completed-height metadata unavailable: {e}");
                    None
                });
            Ok(SyncProgress {
                scanned_height,
                chain_tip_height,
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
            is_complete: false,
        }),
    }
}

pub(crate) use proposal_locks::recover_previous_process;

// ======================== Re-exports from send.rs ========================
pub(crate) use send::resubmit_pending_transactions;
pub use send::{
    estimate_fee, execute_proposal_with_seed_loader, propose_send, propose_shield,
    ExecuteProposalResult, SHIELDING_THRESHOLD,
};

// ======================== Sync State & Orchestration ========================

/// How much of the wallet, from its birthday to the chain tip, has been
/// scanned, from 0 to 1, or none before the first sync.
pub fn scan_progress(db_path: &str, network: WalletNetwork) -> Result<Option<f64>, String> {
    let Some(summary) = crate::db::wallet_summary(db_path, network)? else {
        return Ok(None);
    };
    let progress = summary.progress();
    let (scanned, total) = progress
        .recovery()
        .into_iter()
        .chain([progress.scan()])
        .fold((0, 0), |(n, d), r| (n + r.numerator(), d + r.denominator()));
    Ok((total > 0).then(|| scanned as f64 / total as f64))
}
