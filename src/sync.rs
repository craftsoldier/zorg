use zcash_client_backend::data_api::wallet::ConfirmationsPolicy;

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

/// The wallet's fully scanned height and chain tip height, or none before the
/// wallet has a chain tip and birthday.
pub(crate) fn wallet_scan_heights(db: &WalletDatabase) -> Result<Option<(u64, u64)>, String> {
    Ok(db
        .get_wallet_snapshot(ConfirmationsPolicy::default())
        .map_err(|e| e.to_string())?
        .map(|snapshot| {
            (
                u32::from(snapshot.fully_scanned_height()) as u64,
                u32::from(snapshot.chain_tip_height()) as u64,
            )
        }))
}

pub fn get_sync_progress(db_path: &str, network: WalletNetwork) -> Result<SyncProgress, String> {
    let db = open_wallet_db_for_read(db_path, network)?;
    match wallet_scan_heights(&db)? {
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

/// How many notes of the wallet's scan window have been scanned, as
/// `(scanned, total)`, or none before the first sync. For display only.
pub fn scan_progress(db_path: &str, network: WalletNetwork) -> Result<Option<(u64, u64)>, String> {
    let Some(summary) = crate::db::wallet_summary(db_path, network)? else {
        return Ok(None);
    };
    let progress = summary.progress();
    let (scanned, total) = progress
        .recovery()
        .into_iter()
        .chain([progress.scan()])
        .fold((0, 0), |(n, d), r| (n + r.numerator(), d + r.denominator()));
    Ok((total > 0).then_some((scanned, total)))
}
