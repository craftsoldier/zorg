use std::collections::HashSet;

use nonempty::NonEmpty;
use rusqlite::{params, OptionalExtension};
use shardtree::error::{InsertionError, QueryError, ShardTreeError};
use tonic::transport::Channel;
use zcash_client_backend::data_api::{
    chain::{self, error::Error as ChainError, scan_cached_blocks},
    scanning::{ScanPriority, ScanRange},
    wallet::ConfirmationsPolicy,
    AccountBirthday, WalletCommitmentTrees, WalletRead, WalletWrite,
};
use zcash_client_sqlite::error::SqliteClientError;
use zcash_primitives::block::BlockHash;
use zcash_protocol::consensus::{BlockHeight, NetworkUpgrade, Parameters};

use crate::{
    db::{
        open_readonly_conn_with_timeout, open_wallet_db_with_timeout,
        open_wallet_raw_conn_with_timeout, with_wallet_db_write_lock, WalletDatabase,
        SYNC_DB_BUSY_TIMEOUT,
    },
    network::WalletNetwork,
    sync,
};

use {
    ::transparent::{
        address::Script,
        bundle::{OutPoint, TxOut},
    },
    zcash_client_backend::{
        proto::service::compact_tx_streamer_client::CompactTxStreamerClient,
        wallet::WalletTransparentOutput,
    },
    zcash_keys::encoding::AddressCodec as _,
    zcash_protocol::value::Zatoshis,
};

mod block_source;
mod enhance;
mod error;
pub use error::is_endpoint_failover_candidate_message;
mod lwd;

use enhance::run_enhancement;
pub(crate) use error::SyncError;
use error::{RecoveryStrategy, MAX_REWINDS_PER_RUN};
pub(crate) use lwd::{connect, get_latest_block, next_stream_message, send_transaction};
use lwd::{download_blocks, download_subtree_roots, get_address_utxos_stream, get_tree_state};

/// Progress event sent to caller.
#[derive(Clone, Debug)]
pub struct SyncProgressEvent {
    pub scanned_height: u64,
    pub chain_tip_height: u64,
}

const BATCH_SIZE: u32 = 2000;

/// Sandblasting attack range (Zcash mainnet). Blocks in this range
/// contain a very large number of outputs from a sustained spam
/// attack, making `scan_cached_blocks` significantly more expensive
/// per block. We reduce the batch size to `BATCH_SIZE_SANDBLASTING`
/// when the batch starts inside this window to avoid excessive
/// memory pressure and potential timeouts; batches before it end
/// at the window's start.
///
/// Matches `zcash-android-wallet-sdk`'s `SANDBLASTING_RANGE` in
/// `CompactBlockProcessor.kt:1171-1181`.
const SANDBLASTING_START: u32 = 1_710_000;
const SANDBLASTING_END: u32 = 2_050_000;
const BATCH_SIZE_SANDBLASTING: u32 = 100;

const MAX_WITNESS_REPAIR_PASSES_PER_RUN: u32 = 3;
const WITNESS_CHECK_MAX_CLEAN_AGE_BLOCKS: u64 = 10_000;
const SYNC_META_TABLE: &str = "ext_zorg_sync_meta";
const LAST_COMPLETED_SYNC_HEIGHT_KEY: &str = "last_completed_sync_height";
const SYNC_IN_PROGRESS_KEY: &str = "sync_in_progress";
const LAST_COMPLETED_SYNC_AT_KEY: &str = "last_completed_sync_at";
const WITNESS_CHECK_LAST_CLEAN_HEIGHT_KEY: &str = "witness_check_last_clean_height";
type SyncCompletionMeta = (Option<u64>, Option<bool>);
// `truncate_to_chain_state` only injects a canonical frontier when the requested
// height is below the retained checkpoint window. Start at the pruning depth
// and escalate so corrupted anchor checkpoints do not survive the repair.
const ANCHOR_ROOT_REPAIR_REWIND_DISTANCES: [u32; 3] = [100, 1000, 10_000];

/// Sync-scoped elapsed time reference. Set at sync start.
static SYNC_START: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

pub(crate) fn elapsed() -> String {
    SYNC_START
        .lock()
        .ok()
        .and_then(|g| g.map(|t| format!("{:.1}s", t.elapsed().as_secs_f64())))
        .unwrap_or_default()
}

fn batch_size_for_range(base_batch_size: u32, start: BlockHeight) -> u32 {
    let start = u32::from(start);
    if start >= SANDBLASTING_END {
        base_batch_size
    } else if start >= SANDBLASTING_START {
        BATCH_SIZE_SANDBLASTING
    } else {
        base_batch_size.min(SANDBLASTING_START - start)
    }
}

fn chain_tip_exclusive_end(current_tip_height: u64) -> BlockHeight {
    let current_tip = u32::try_from(current_tip_height).unwrap_or(u32::MAX);
    BlockHeight::from_u32(current_tip.saturating_add(1))
}

fn scannable_batch_end(
    base_batch_size: u32,
    start: BlockHeight,
    range_end: BlockHeight,
    current_tip_height: u64,
) -> Option<(u32, BlockHeight)> {
    let available_end = std::cmp::min(range_end, chain_tip_exclusive_end(current_tip_height));
    if start >= available_end {
        return None;
    }

    let batch_size = batch_size_for_range(base_batch_size, start);
    let end = std::cmp::min(start + batch_size, available_end);
    Some((batch_size, end))
}

fn effective_base_batch_size(default_batch_size: u32) -> u32 {
    #[cfg(debug_assertions)]
    {
        if let Ok(raw) = std::env::var("ZCASH_E2E_SYNC_BATCH_SIZE") {
            if let Ok(parsed) = raw.parse::<u32>() {
                if parsed > 0 {
                    return parsed.min(default_batch_size);
                }
            }
        }
    }

    default_batch_size
}

#[cfg(debug_assertions)]
async fn maybe_sleep_for_e2e_sync_batch_delay() {
    let Ok(raw) = std::env::var("ZCASH_E2E_SYNC_BATCH_DELAY_MS") else {
        return;
    };
    let Ok(parsed) = raw.parse::<u64>() else {
        return;
    };
    if parsed == 0 {
        return;
    }

    tokio::time::sleep(std::time::Duration::from_millis(parsed.min(5_000))).await;
}

fn is_pending_scan_range(range: &ScanRange) -> bool {
    range.priority() != ScanPriority::Ignored && range.priority() != ScanPriority::Scanned
}

fn recovery_resubmit_exclusions(
    db_path: &str,
    ranges: &[ScanRange],
) -> Result<HashSet<Vec<u8>>, SyncError> {
    let pending_ranges = ranges
        .iter()
        .filter(|range| is_pending_scan_range(range))
        .map(|range| range.block_range().clone())
        .collect::<Vec<_>>();
    crate::sync::get_unmined_txids_with_mined_output_evidence(db_path, &pending_ranges)
        .map_err(SyncError::db)
}

fn pending_scan_blocks(ranges: &[ScanRange]) -> u64 {
    ranges
        .iter()
        .filter(|r| is_pending_scan_range(r))
        .map(|r| {
            u32::from(r.block_range().end).saturating_sub(u32::from(r.block_range().start)) as u64
        })
        .sum()
}

fn first_pending_scan_range(ranges: &[ScanRange]) -> Option<String> {
    ranges
        .iter()
        .find(|r| is_pending_scan_range(r))
        .map(|r| r.to_string())
}

fn block_range_len(range: &std::ops::Range<BlockHeight>) -> u64 {
    u32::from(range.end).saturating_sub(u32::from(range.start)) as u64
}

fn describe_block_range(range: &std::ops::Range<BlockHeight>) -> String {
    format!("{}..{}", u32::from(range.start), u32::from(range.end))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct WitnessCheckMeta {
    last_clean_height: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WitnessCheckRunReason {
    Forced,
    MissingMarker,
    TipBelowLastClean { last_clean_height: u64 },
    MaxCleanAgeReached { age_blocks: u64 },
    MetadataUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WitnessCheckDecision {
    Run(WitnessCheckRunReason),
    Skip {
        last_clean_height: u64,
        age_blocks: u64,
    },
}

impl WitnessCheckRunReason {
    fn description(self) -> String {
        match self {
            WitnessCheckRunReason::Forced => "forced by repair/reorg signal".into(),
            WitnessCheckRunReason::MissingMarker => "no clean marker".into(),
            WitnessCheckRunReason::TipBelowLastClean { last_clean_height } => format!(
                "tip moved below last clean height (last_clean_height={last_clean_height})"
            ),
            WitnessCheckRunReason::MaxCleanAgeReached { age_blocks } => format!(
                "clean marker is stale (age_blocks={age_blocks}, max_age_blocks={WITNESS_CHECK_MAX_CLEAN_AGE_BLOCKS})"
            ),
            WitnessCheckRunReason::MetadataUnavailable => "metadata unavailable".into(),
        }
    }
}

fn decide_witness_check(
    meta: WitnessCheckMeta,
    current_tip_height: u64,
    force_check: bool,
) -> WitnessCheckDecision {
    if force_check {
        return WitnessCheckDecision::Run(WitnessCheckRunReason::Forced);
    }

    let Some(last_clean_height) = meta.last_clean_height else {
        return WitnessCheckDecision::Run(WitnessCheckRunReason::MissingMarker);
    };

    if last_clean_height > current_tip_height {
        return WitnessCheckDecision::Run(WitnessCheckRunReason::TipBelowLastClean {
            last_clean_height,
        });
    }

    let age_blocks = current_tip_height - last_clean_height;
    if age_blocks >= WITNESS_CHECK_MAX_CLEAN_AGE_BLOCKS {
        return WitnessCheckDecision::Run(WitnessCheckRunReason::MaxCleanAgeReached { age_blocks });
    }

    WitnessCheckDecision::Skip {
        last_clean_height,
        age_blocks,
    }
}

fn sync_meta_table_exists(conn: &rusqlite::Connection) -> Result<bool, String> {
    conn.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1
        )",
        params![SYNC_META_TABLE],
        |row| row.get::<_, i64>(0),
    )
    .map(|exists| exists != 0)
    .map_err(|e| format!("read sync metadata table existence: {e}"))
}

fn read_sync_meta_value(conn: &rusqlite::Connection, key: &str) -> Result<Option<String>, String> {
    conn.query_row(
        "SELECT value FROM ext_zorg_sync_meta WHERE key = ?1",
        params![key],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(|e| format!("read sync metadata value {key}: {e}"))
}

fn parse_sync_meta_u32(key: &str, value: Option<String>) -> Option<u32> {
    let value = value?;
    match value.parse::<u32>() {
        Ok(parsed) => Some(parsed),
        Err(e) => {
            log::warn!("sync: ignoring invalid sync metadata value {key}={value:?}: {e}");
            None
        }
    }
}

fn parse_sync_meta_u64(key: &str, value: Option<String>) -> Option<u64> {
    let value = value?;
    match value.parse::<u64>() {
        Ok(parsed) => Some(parsed),
        Err(e) => {
            log::warn!("sync: ignoring invalid sync metadata value {key}={value:?}: {e}");
            None
        }
    }
}

fn read_witness_check_meta(db_data_path: &str) -> Result<WitnessCheckMeta, String> {
    let conn = open_readonly_conn_with_timeout(db_data_path, Some(SYNC_DB_BUSY_TIMEOUT))?;
    if !sync_meta_table_exists(&conn)? {
        return Ok(WitnessCheckMeta::default());
    }

    Ok(WitnessCheckMeta {
        last_clean_height: parse_sync_meta_u64(
            WITNESS_CHECK_LAST_CLEAN_HEIGHT_KEY,
            read_sync_meta_value(&conn, WITNESS_CHECK_LAST_CLEAN_HEIGHT_KEY)?,
        ),
    })
}

fn read_sync_completion_meta(db_data_path: &str) -> Result<SyncCompletionMeta, String> {
    let conn = open_readonly_conn_with_timeout(db_data_path, Some(SYNC_DB_BUSY_TIMEOUT))?;
    if !sync_meta_table_exists(&conn)? {
        return Ok((None, None));
    }

    Ok((
        parse_sync_meta_u64(
            LAST_COMPLETED_SYNC_HEIGHT_KEY,
            read_sync_meta_value(&conn, LAST_COMPLETED_SYNC_HEIGHT_KEY)?,
        ),
        parse_sync_meta_u32(
            SYNC_IN_PROGRESS_KEY,
            read_sync_meta_value(&conn, SYNC_IN_PROGRESS_KEY)?,
        )
        .map(|value| value != 0),
    ))
}

fn witness_check_decision(
    db_data_path: &str,
    current_tip_height: u64,
    force_check: bool,
) -> WitnessCheckDecision {
    match read_witness_check_meta(db_data_path) {
        Ok(meta) => decide_witness_check(meta, current_tip_height, force_check),
        Err(e) => {
            log::warn!(
                "[{}] sync: witness repair metadata unavailable, running check: {e}",
                elapsed(),
            );
            WitnessCheckDecision::Run(WitnessCheckRunReason::MetadataUnavailable)
        }
    }
}

fn ensure_sync_meta_table(conn: &rusqlite::Connection) -> Result<(), String> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS ext_zorg_sync_meta (
            key TEXT PRIMARY KEY NOT NULL,
            value TEXT NOT NULL
        )",
        [],
    )
    .map_err(|e| format!("create sync metadata table: {e}"))?;
    Ok(())
}

fn mark_witness_check_clean(db_data_path: &str, current_tip_height: u64) -> Result<(), String> {
    let mut conn = open_wallet_raw_conn_with_timeout(db_data_path, SYNC_DB_BUSY_TIMEOUT)?;
    ensure_sync_meta_table(&conn)?;

    let tx = conn
        .transaction()
        .map_err(|e| format!("begin sync metadata transaction: {e}"))?;
    tx.execute(
        "INSERT INTO ext_zorg_sync_meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![
            WITNESS_CHECK_LAST_CLEAN_HEIGHT_KEY,
            current_tip_height.to_string()
        ],
    )
    .map_err(|e| format!("write witness check clean height: {e}"))?;
    tx.commit()
        .map_err(|e| format!("commit sync metadata transaction: {e}"))
}

pub(crate) fn completed_sync_height_for_status(
    db_data_path: &str,
    scanned_height: u64,
    chain_tip_height: u64,
) -> Result<Option<u64>, String> {
    let (completed_height, in_progress) = read_sync_completion_meta(db_data_path)?;
    Ok(if in_progress == Some(true) {
        None
    } else {
        completed_height.or_else(|| {
            (chain_tip_height > 0 && scanned_height >= chain_tip_height).then_some(chain_tip_height)
        })
    })
}

fn mark_sync_started(db_data_path: &str) -> Result<(), String> {
    let mut conn = open_wallet_raw_conn_with_timeout(db_data_path, SYNC_DB_BUSY_TIMEOUT)?;
    ensure_sync_meta_table(&conn)?;
    let tx = conn
        .transaction()
        .map_err(|e| format!("begin sync-start metadata transaction: {e}"))?;
    tx.execute(
        "INSERT INTO ext_zorg_sync_meta(key, value) VALUES (?1, '1')
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![SYNC_IN_PROGRESS_KEY],
    )
    .map_err(|e| format!("write sync in-progress marker: {e}"))?;
    tx.commit()
        .map_err(|e| format!("commit sync-start metadata: {e}"))
}

fn mark_sync_completed(db_data_path: &str, completed_tip_height: u64) -> Result<(), String> {
    let mut conn = open_wallet_raw_conn_with_timeout(db_data_path, SYNC_DB_BUSY_TIMEOUT)?;
    ensure_sync_meta_table(&conn)?;
    let tx = conn
        .transaction()
        .map_err(|e| format!("begin completed sync transaction: {e}"))?;
    tx.execute(
        "INSERT INTO ext_zorg_sync_meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![
            LAST_COMPLETED_SYNC_HEIGHT_KEY,
            completed_tip_height.to_string()
        ],
    )
    .map_err(|e| format!("write completed sync height: {e}"))?;
    tx.execute(
        "INSERT INTO ext_zorg_sync_meta(key, value) VALUES (?1, '0')
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![SYNC_IN_PROGRESS_KEY],
    )
    .map_err(|e| format!("clear sync in-progress marker: {e}"))?;
    let completed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    tx.execute(
        "INSERT INTO ext_zorg_sync_meta(key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![LAST_COMPLETED_SYNC_AT_KEY, completed_at.to_string()],
    )
    .map_err(|e| format!("write last-completed-sync timestamp: {e}"))?;
    tx.commit()
        .map_err(|e| format!("commit completed sync transaction: {e}"))
}

/// Unix time of the last completed sync, if the wallet has ever synced.
pub fn last_completed_sync_at(db_path: &str) -> Result<Option<u64>, String> {
    let conn = open_readonly_conn_with_timeout(db_path, Some(SYNC_DB_BUSY_TIMEOUT))?;
    if !sync_meta_table_exists(&conn)? {
        return Ok(None);
    }
    Ok(parse_sync_meta_u64(
        LAST_COMPLETED_SYNC_AT_KEY,
        read_sync_meta_value(&conn, LAST_COMPLETED_SYNC_AT_KEY)?,
    ))
}

fn ensure_complete_scan_state(
    db: &mut WalletDatabase,
    current_tip_height: u64,
) -> Result<(u64, u64), SyncError> {
    let ranges = db
        .suggest_scan_ranges()
        .map_err(|e| SyncError::db(format!("suggest_scan_ranges: {e}")))?;
    let pending_blocks = pending_scan_blocks(&ranges);
    if pending_blocks > 0 {
        let first = first_pending_scan_range(&ranges).unwrap_or_else(|| "unknown".into());
        return Err(SyncError::continuity(
            current_tip_height,
            format!(
                "sync completion blocked: {pending_blocks} pending scan blocks remain \
                 (first pending range: {first})"
            ),
        ));
    }

    let Some((fully_scanned_height, db_tip_height)) =
        sync::wallet_scan_heights(db).map_err(SyncError::db)?
    else {
        if current_tip_height == 0 {
            return Ok((0, 0));
        }
        return Err(SyncError::db(format!(
            "sync completion blocked: wallet summary unavailable at tip {current_tip_height}"
        )));
    };

    if db_tip_height < current_tip_height {
        return Err(SyncError::continuity(
            current_tip_height,
            format!(
                "sync completion blocked: wallet DB chain tip {db_tip_height} \
                 lags lightwalletd tip {current_tip_height}"
            ),
        ));
    }

    if fully_scanned_height < db_tip_height {
        return Err(SyncError::continuity(
            db_tip_height,
            format!(
                "sync completion blocked: fully scanned height {fully_scanned_height} \
                 below wallet DB chain tip {db_tip_height}"
            ),
        ));
    }

    Ok((fully_scanned_height, db_tip_height))
}

fn queue_witness_repairs_if_needed(
    db_data_path: &str,
    db: &mut WalletDatabase,
    current_tip_height: u64,
    repair_passes_this_run: &mut u32,
    force_check: bool,
) -> Result<Option<u64>, SyncError> {
    match witness_check_decision(db_data_path, current_tip_height, force_check) {
        WitnessCheckDecision::Run(reason) => {
            log::info!(
                "[{}] sync: witness repair check running ({})",
                elapsed(),
                reason.description(),
            );
        }
        WitnessCheckDecision::Skip {
            last_clean_height,
            age_blocks,
        } => {
            log::info!(
                "[{}] sync: witness repair check skipped \
                 (last_clean_height={last_clean_height}, current_tip={current_tip_height}, \
                 age_blocks={age_blocks}, max_age_blocks={WITNESS_CHECK_MAX_CLEAN_AGE_BLOCKS})",
                elapsed(),
            );
            return Ok(None);
        }
    }

    let rescan_ranges = with_wallet_db_write_lock("sync_engine.check_witnesses", || {
        match db.check_witnesses() {
            Ok(ranges) => Ok(ranges),
            Err(error) if is_witness_position_beyond_tree(&error) => {
                let cleared = clear_unmined_note_commitment_positions(db_data_path)?;
                if cleared == 0 {
                    return Err(SyncError::db(format!("check_witnesses: {error}")));
                }

                log::warn!(
                    "[{}] sync: cleared {} stale commitment-tree position(s) from unmined notes after reorg; retrying witness check",
                    elapsed(),
                    cleared,
                );
                db.check_witnesses().map_err(|retry_error| {
                    SyncError::db(format!(
                        "check_witnesses after clearing unmined note positions: {retry_error}"
                    ))
                })
            }
            Err(error) => Err(SyncError::db(format!("check_witnesses: {error}"))),
        }
    })?;

    let Some(nonempty_ranges) = NonEmpty::from_vec(rescan_ranges) else {
        if let Err(e) = with_wallet_db_write_lock("sync_engine.mark_witness_check_clean", || {
            mark_witness_check_clean(db_data_path, current_tip_height)
        }) {
            log::warn!(
                "[{}] sync: witness repair clean marker update failed: {e}",
                elapsed(),
            );
        } else {
            log::info!(
                "[{}] sync: witness repair check found no work; marked clean at height {}",
                elapsed(),
                current_tip_height,
            );
        }
        return Ok(None);
    };

    if *repair_passes_this_run >= MAX_WITNESS_REPAIR_PASSES_PER_RUN {
        let first = describe_block_range(&nonempty_ranges.head);
        return Err(SyncError::db(format!(
            "sync completion blocked: witness repair budget exhausted \
             after {} pass(es); first remaining repair range: {first}",
            MAX_WITNESS_REPAIR_PASSES_PER_RUN,
        )));
    }

    *repair_passes_this_run += 1;
    let pass = *repair_passes_this_run;
    let range_count = 1 + nonempty_ranges.tail.len();
    let repair_blocks = nonempty_ranges.iter().map(block_range_len).sum::<u64>();
    let first = describe_block_range(&nonempty_ranges.head);

    log::warn!(
        "[{}] sync: witness repair pass {}/{} queued {} range(s), {} block(s) \
         (first={first})",
        elapsed(),
        pass,
        MAX_WITNESS_REPAIR_PASSES_PER_RUN,
        range_count,
        repair_blocks,
    );

    with_wallet_db_write_lock("sync_engine.queue_witness_repairs", || {
        db.queue_rescans(nonempty_ranges, ScanPriority::Verify)
            .map_err(|e| SyncError::db(format!("queue witness rescans: {e}")))
    })?;

    let post_ranges = db
        .suggest_scan_ranges()
        .map_err(|e| SyncError::db(format!("suggest_scan_ranges after witness repair: {e}")))?;
    let pending_blocks = pending_scan_blocks(&post_ranges);
    if pending_blocks == 0 && current_tip_height > 0 {
        return Err(SyncError::db(format!(
            "sync completion blocked: witness repair queued ranges but no pending scan \
             ranges were produced at tip {current_tip_height}"
        )));
    }

    Ok(Some(pending_blocks))
}

async fn repair_anchor_root_mismatch_if_needed(
    client: &mut CompactTxStreamerClient<Channel>,
    db: &mut WalletDatabase,
    current_tip_height: u64,
    repair_passes_this_run: &mut u32,
) -> Result<Option<u64>, SyncError> {
    let Some((target_height, anchor_height)) = db
        .get_target_and_anchor_heights(ConfirmationsPolicy::default().trusted())
        .map_err(|e| SyncError::db(format!("get_target_and_anchor_heights: {e}")))?
    else {
        return Ok(None);
    };
    // A height-0 anchor is the empty birthday tree, and lightwalletd reads height 0 as unset.
    if u32::from(anchor_height) == 0 {
        return Ok(None);
    }

    let local_sapling = db
        .with_sapling_tree_mut(|tree| tree.root_at_checkpoint_id(&anchor_height))
        .map_err(|e| SyncError::db(format!("sapling root at {anchor_height}: {e}")))?;
    let local_orchard = db
        .with_orchard_tree_mut(|tree| tree.root_at_checkpoint_id(&anchor_height))
        .map_err(|e| SyncError::db(format!("orchard root at {anchor_height}: {e}")))?;
    let local_ironwood = db
        .with_ironwood_tree_mut(|tree| tree.root_at_checkpoint_id(&anchor_height))
        .map_err(|e| SyncError::db(format!("ironwood root at {anchor_height}: {e}")))?;

    let anchor_chain_state = get_tree_state(client, u32::from(anchor_height) as u64)
        .await?
        .to_chain_state()
        .map_err(|e| SyncError::parse(format!("parse anchor tree state: {e}")))?;
    if anchor_chain_state.block_height() != anchor_height {
        return Err(SyncError::parse(format!(
            "lightwalletd returned tree state for height {}, requested {anchor_height}",
            anchor_chain_state.block_height(),
        )));
    }

    let canonical_sapling = anchor_chain_state.final_sapling_tree().root();
    let canonical_orchard = anchor_chain_state.final_orchard_tree().root();
    let canonical_ironwood = anchor_chain_state.final_ironwood_tree().root();
    let ironwood_roots_match =
        local_ironwood.as_ref().and_then(|root| root.as_ref()) == Some(&canonical_ironwood);
    if local_sapling.as_ref() == Some(&canonical_sapling)
        && local_orchard.as_ref() == Some(&canonical_orchard)
        && ironwood_roots_match
    {
        return Ok(None);
    }

    let start_idx = usize::try_from(*repair_passes_this_run).unwrap_or(usize::MAX);
    let mut last_root_conflict = None;
    for rewind_distance in ANCHOR_ROOT_REPAIR_REWIND_DISTANCES
        .iter()
        .copied()
        .skip(start_idx)
    {
        *repair_passes_this_run += 1;
        let repair_height = anchor_height.saturating_sub(rewind_distance);
        let repair_chain_state = get_tree_state(client, u32::from(repair_height) as u64)
            .await?
            .to_chain_state()
            .map_err(|e| SyncError::parse(format!("parse repair tree state: {e}")))?;
        if repair_chain_state.block_height() != repair_height {
            return Err(SyncError::parse(format!(
                "lightwalletd returned tree state for height {}, requested {repair_height}",
                repair_chain_state.block_height(),
            )));
        }

        log::warn!(
            "[{}] sync: anchor root mismatch at {anchor_height} \
             (target={}, repair_height={repair_height}, pass {}/{}); \
             local_sapling={:?}, canonical_sapling={:?}, local_orchard={:?}, \
             canonical_orchard={:?}, local_ironwood={:?}, canonical_ironwood={:?}; \
             rewinding to canonical chain state",
            elapsed(),
            u32::from(target_height),
            *repair_passes_this_run,
            ANCHOR_ROOT_REPAIR_REWIND_DISTANCES.len(),
            local_sapling,
            canonical_sapling,
            local_orchard,
            canonical_orchard,
            local_ironwood,
            canonical_ironwood,
        );

        let current_tip = BlockHeight::from_u32(current_tip_height as u32);
        let attempt_result = with_wallet_db_write_lock(
            "sync_engine.truncate_to_chain_state.anchor_root_mismatch",
            || -> Result<Result<Vec<ScanRange>, String>, SyncError> {
                match db.truncate_to_chain_state(repair_chain_state.clone()) {
                    Ok(()) => {}
                    Err(e) if is_commitment_tree_root_conflict(&e) => {
                        return Ok(Err(format!("{e}")));
                    }
                    Err(e) if is_sqlite_lock_contention(&e) => {
                        return Err(SyncError::other(format!(
                            "truncate_to_chain_state({repair_height}): SQLite lock contention: {e}"
                        )));
                    }
                    Err(e) => {
                        return Err(SyncError::db(format!(
                            "truncate_to_chain_state({repair_height}): {e}"
                        )));
                    }
                }
                db.update_chain_tip(current_tip).map_err(|e| {
                    SyncError::db(format!(
                        "update_chain_tip({current_tip_height}) after anchor root repair: {e}"
                    ))
                })?;
                db.suggest_scan_ranges()
                    .map_err(|e| {
                        SyncError::db(format!("suggest_scan_ranges after anchor root repair: {e}"))
                    })
                    .map(Ok)
            },
        )?;

        let post_rewind_ranges = match attempt_result {
            Ok(ranges) => ranges,
            Err(conflict) => {
                log::warn!(
                    "[{}] sync: anchor root repair at {repair_height} conflicted \
                     with an existing tree root; trying a deeper repair if available ({conflict})",
                    elapsed(),
                );
                last_root_conflict = Some(conflict);
                continue;
            }
        };

        let pending_blocks = pending_scan_blocks(&post_rewind_ranges);
        let first_pending =
            first_pending_scan_range(&post_rewind_ranges).unwrap_or_else(|| "none".into());
        log::info!(
            "[{}] sync: anchor root repair queued {pending_blocks} block(s) \
             (first_pending={first_pending})",
            elapsed(),
        );

        let anchor_height_u64 = u32::from(anchor_height) as u64;
        if pending_blocks == 0 && anchor_height_u64 < current_tip_height {
            return Err(SyncError::continuity(
                current_tip_height,
                format!(
                    "anchor root repair at {anchor_height} produced no pending scan \
                     ranges, but lightwalletd tip is {current_tip_height}"
                ),
            ));
        }

        return Ok(Some(pending_blocks));
    }

    Err(SyncError::db(format!(
        "sync completion blocked: anchor root repair budget exhausted \
         after {} pass(es) at anchor {anchor_height}{}",
        ANCHOR_ROOT_REPAIR_REWIND_DISTANCES.len(),
        last_root_conflict
            .as_deref()
            .map(|e| format!("; last root conflict: {e}"))
            .unwrap_or_default(),
    )))
}

async fn refresh_utxos(
    client: &mut CompactTxStreamerClient<Channel>,
    db: &mut WalletDatabase,
    network: WalletNetwork,
) -> Result<(), SyncError> {
    let query_network = network;
    for account_id in db
        .get_account_ids()
        .map_err(|e| SyncError::db(format!("get_account_ids: {e}")))?
    {
        let start_height = db
            .utxo_query_height(account_id)
            .map_err(|e| SyncError::db(format!("utxo_query_height: {e}")))?;

        let addresses: Vec<String> = db
            .get_transparent_receivers(account_id, true, true)
            .map_err(|e| SyncError::db(format!("get_transparent_receivers: {e}")))?
            .into_keys()
            .map(|addr| addr.encode(&query_network))
            .collect();

        if addresses.is_empty() {
            continue;
        }

        log::info!(
            "[{}] sync: refreshing transparent UTXOs from height {} ({} addresses)",
            elapsed(),
            u32::from(start_height),
            addresses.len(),
        );

        let mut stream = get_address_utxos_stream(client, addresses, start_height).await?;

        loop {
            let reply = next_stream_message(&mut stream, "get_address_utxos_stream").await?;
            let Some(reply) = reply else {
                break;
            };
            let txid: [u8; 32] = reply
                .txid
                .try_into()
                .map_err(|_| SyncError::parse("transparent UTXO txid was not 32 bytes"))?;
            let index = u32::try_from(reply.index).map_err(|_| {
                SyncError::parse(format!("invalid transparent UTXO index: {}", reply.index))
            })?;
            let height = u32::try_from(reply.height).map_err(|_| {
                SyncError::parse(format!("invalid transparent UTXO height: {}", reply.height))
            })?;
            let value = Zatoshis::from_nonnegative_i64(reply.value_zat).map_err(|_| {
                SyncError::parse(format!(
                    "invalid transparent UTXO value: {}",
                    reply.value_zat
                ))
            })?;

            let mut script = Script::default();
            script.0 .0 = reply.script;
            let output = WalletTransparentOutput::from_parts(
                OutPoint::new(txid, index),
                TxOut::new(value, script),
                Some(BlockHeight::from_u32(height)),
                None,
                None,
                None,
            )
            .ok_or_else(|| {
                SyncError::parse("transparent UTXO script did not decode to a wallet address")
            })?;

            with_wallet_db_write_lock("sync_engine.put_received_transparent_utxo", || {
                db.put_received_transparent_utxo(&output)
                    .map_err(|e| SyncError::db(format!("put_received_transparent_utxo: {e}")))
            })?;
        }
    }
    Ok(())
}

// ==================== Main sync ====================

/// Run the full sync loop with automatic retry on failure.
/// Retries up to 3 times with exponential backoff (2s, 4s, 8s).
pub async fn run_sync_inner(
    db_data_path: &str,
    lightwalletd_url: &str,
    network: WalletNetwork,
    progress_fn: impl Fn(SyncProgressEvent) + Send + Sync,
) -> Result<(), String> {
    const MAX_RETRIES: u32 = 3;
    let mut last_err = String::new();
    *SYNC_START.lock().unwrap() = Some(std::time::Instant::now());

    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            let delay_secs = 1u64 << attempt; // 2, 4, 8
            log::warn!(
                "[{}] sync: retry {}/{} in {}s (error: {})",
                elapsed(),
                attempt,
                MAX_RETRIES,
                delay_secs,
                last_err
            );
            tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
        }

        match run_sync_impl(db_data_path, lightwalletd_url, network, &progress_fn).await {
            Ok(()) => return Ok(()),
            Err(sync_err) => {
                // Inspect the typed error's recovery strategy before
                // flattening to a `String` at the public boundary. Fatal
                // variants (`Db`, `Parse`) bail out immediately with no
                // retry — repeatedly hammering a DB corruption or a
                // deserialization bug doesn't fix it and just costs time.
                // Transient variants (`Network`, `Other`) fall through to
                // the existing exponential-backoff retry path.
                //
                // A `Rewind` strategy reaching this layer means the inline
                // reorg-recovery inside `run_sync_impl` exhausted its
                // phase budget (commit 1.4). Treat it as a retry-worthy
                // transient: the next attempt gets a fresh rewind budget,
                // which is often enough to get past a multi-level reorg
                // that couldn't be cleared in one run.
                let strategy = sync_err.recovery_strategy();
                let err_string = sync_err.to_string();
                match strategy {
                    RecoveryStrategy::Fatal => {
                        log::error!(
                            "[{}] sync: fatal error, not retrying: {err_string}",
                            elapsed(),
                        );
                        return Err(err_string);
                    }
                    RecoveryStrategy::RetryWithBackoff | RecoveryStrategy::Rewind { .. } => {
                        last_err = err_string;
                        if attempt == MAX_RETRIES {
                            log::error!(
                                "[{}] sync: all {} retries exhausted",
                                elapsed(),
                                MAX_RETRIES,
                            );
                        }
                    }
                }
            }
        }
    }

    Err(last_err)
}

/// Inner sync implementation. Called by run_sync_inner (with retry wrapper).
async fn run_sync_impl(
    db_data_path: &str,
    lightwalletd_url: &str,
    network: WalletNetwork,
    progress_fn: &(impl Fn(SyncProgressEvent) + Send + Sync),
) -> Result<(), SyncError> {
    let default_batch_size = BATCH_SIZE;
    let base_batch_size = effective_base_batch_size(default_batch_size);
    log::info!(
        "[{}] sync: starting (base_batch={})",
        elapsed(),
        base_batch_size,
    );

    // Persist the active session before any new sync work begins. A crash, or
    // the process being killed or replaced mid-sync, cannot leave the previous
    // completed tip looking like the current run completed successfully.
    with_wallet_db_write_lock("sync_engine.mark_sync_started", || {
        mark_sync_started(db_data_path)
    })
    .map_err(SyncError::db)?;

    // 1. Connect gRPC (plain TLS via tonic + webpki roots).
    let mut client = connect(network, lightwalletd_url).await?;

    // Open DB once — reused for the entire sync
    let mut db =
        with_wallet_db_write_lock("sync_engine.open_db", || open_db(db_data_path, network))?;

    // 2. Get chain tip. `current_tip_height` is updated by the tip
    // fetch after each batch so that progress events always reflect
    // the latest known chain height, not the one captured at sync
    // start. The initial `tip` response is also kept around for its
    // other fields but `current_tip_height` is the authoritative
    // value for emitted events.
    let tip = get_latest_block(&mut client).await?;
    let mut current_tip_height: u64 = tip.height;
    let tip_height = BlockHeight::from_u32(tip.height as u32);
    log::info!("[{}] sync: chain tip = {}", elapsed(), tip.height);

    with_wallet_db_write_lock("sync_engine.update_chain_tip.initial", || {
        db.update_chain_tip(tip_height).map_err(|e| {
            if is_sqlite_lock_contention(&e) {
                SyncError::other(format!(
                    "update_chain_tip: transient SQLite lock contention: {e}"
                ))
            } else {
                SyncError::db(format!("update_chain_tip: {e}"))
            }
        })
    })?;

    // Retained send-lock expiry requires a usable target height.
    crate::sync::recover_previous_process(db_data_path, network).map_err(SyncError::db)?;

    refresh_utxos(&mut client, &mut db, network).await?;

    // 2.5. Resubmit eligible unmined wallet txs now that we know the
    // current tip. Matches the first of the three
    // resubmit call sites in zcash-android-wallet-sdk's
    // `processNewBlocks` (line 551). Best-effort: failures are
    // logged inside the helper and must not abort the sync.
    //
    // We reuse the same `client` instead of opening a fresh channel.
    let startup_ranges = db
        .suggest_scan_ranges()
        .map_err(|e| SyncError::db(format!("suggest_scan_ranges: {e}")))?;
    let startup_resubmit_exclusions = recovery_resubmit_exclusions(db_data_path, &startup_ranges)?;
    let _ = crate::sync::resubmit_pending_transactions(
        db_data_path,
        &mut client,
        tip.height as u32,
        &startup_resubmit_exclusions,
    )
    .await;

    // 3. Download subtree roots (incremental)
    download_subtree_roots(&mut client, &mut db, db_data_path, network).await?;

    // 4. Calculate initial scan target (before any scanning)
    let initial_ranges = db
        .suggest_scan_ranges()
        .map_err(|e| SyncError::db(format!("suggest_scan_ranges: {e}")))?;
    log::info!(
        "[{}] sync: {} blocks to scan",
        elapsed(),
        pending_scan_blocks(&initial_ranges)
    );

    // Bounded counters for reorg-triggered rewinds inside this one sync run,
    // split between the verify phase and the main scan phase. Separate
    // budgets match zcash-android-wallet-sdk's pattern of running a
    // dedicated verify-first loop before the main scan, so a flapping
    // verify range can't eat the main scan's rewind budget.
    let mut verify_rewinds_this_run: u32 = 0;
    let mut main_rewinds_this_run: u32 = 0;
    let mut witness_repair_passes_this_run: u32 = 0;
    let mut anchor_root_repair_passes_this_run: u32 = 0;
    let mut force_witness_check_this_run = false;

    // Phase-transition markers used only for logging. Progress through the
    // scan queue is implicitly ordered by `ScanPriority::Verify` >
    // everything else, so an explicit state machine isn't needed — we just
    // log when we first see a verify range and when we first see a
    // non-verify range so diagnosis of a reorg-heavy sync is easier.
    let mut verify_phase_announced = false;
    let mut main_phase_announced = false;

    // Prefetched block source from the previous iteration.
    // When the scan loop processes a range that spans multiple batches,
    // we kick off a background download of the next batch while running
    // scan / enhancement / resubmit / progress reporting for the current
    // batch. This overlaps network I/O (download) with CPU-bound
    // work (scan) and unrelated gRPC calls (resubmit), matching
    // the SDK's `.buffer(1)` pipelining pattern in
    // `CompactBlockProcessor.kt:1666`.
    //
    // `None` on the first iteration and whenever the previous batch
    // was the last in its range (so there's nothing to prefetch until
    // `suggest_scan_ranges` runs again).
    type PrefetchResult = Result<
        (
            crate::sync_engine::block_source::MemoryBlockSource,
            zcash_client_backend::proto::service::TreeState,
        ),
        SyncError,
    >;
    /// Prefetched block download state. Implements `Drop` to
    /// abort the spawned tokio task when the loop exits for any
    /// reason (error, break, reorg `continue`) so detached
    /// downloads can't outlive the sync session and leak network
    /// traffic after shutdown.
    struct Prefetch {
        handle: Option<tokio::task::JoinHandle<PrefetchResult>>,
        start: BlockHeight,
        end: BlockHeight,
    }
    impl Drop for Prefetch {
        fn drop(&mut self) {
            if let Some(h) = self.handle.take() {
                h.abort();
            }
        }
    }
    let mut prefetch: Option<Prefetch> = None;

    // 5. Sync loop
    loop {
        let ranges = db
            .suggest_scan_ranges()
            .map_err(|e| SyncError::db(format!("suggest_scan_ranges: {e}")))?;

        let range = match ranges.iter().find(|r| is_pending_scan_range(r)) {
            Some(r) => r.clone(),
            None => {
                if queue_witness_repairs_if_needed(
                    db_data_path,
                    &mut db,
                    current_tip_height,
                    &mut witness_repair_passes_this_run,
                    force_witness_check_this_run,
                )?
                .is_some()
                    || repair_anchor_root_mismatch_if_needed(
                        &mut client,
                        &mut db,
                        current_tip_height,
                        &mut anchor_root_repair_passes_this_run,
                    )
                    .await?
                    .is_some()
                {
                    force_witness_check_this_run = true;
                    prefetch = None;
                    continue;
                } else {
                    ensure_complete_scan_state(&mut db, current_tip_height)?;
                    break;
                }
            }
        };

        // Phase bookkeeping. `ScanPriority::Verify` ranges are
        // librustzcash's "please re-check these blocks to confirm their
        // chain linkage" signal, and always sort ahead of ChainTip /
        // Historic / etc. via `suggest_scan_ranges` (ORDER BY priority
        // DESC), so seeing a non-Verify range means the verify phase has
        // drained. The announcement booleans keep this purely for logs;
        // the rewind counters below are what actually matter.
        let is_verify_phase = range.priority() == ScanPriority::Verify;
        if is_verify_phase && !verify_phase_announced {
            log::info!("[{}] sync: entering verify phase", elapsed());
            verify_phase_announced = true;
        } else if !is_verify_phase && !main_phase_announced {
            if verify_phase_announced {
                log::info!(
                    "[{}] sync: verify phase complete, entering main scan",
                    elapsed()
                );
            } else {
                log::info!(
                    "[{}] sync: entering main scan phase (no verify work)",
                    elapsed()
                );
            }
            main_phase_announced = true;
        }

        let start = range.block_range().start;
        let range_end = range.block_range().end;
        let frontier_height = u32::from(start)
            .checked_sub(1)
            .ok_or_else(|| SyncError::other("scan range starts before a usable frontier"))?;
        // Adaptive batch size: shrink to BATCH_SIZE_SANDBLASTING
        // when the current range overlaps the known Zcash mainnet
        // sandblasting attack window. These blocks contain an
        // order of magnitude more outputs than normal blocks,
        // making scan_cached_blocks much slower per block and
        // using more memory. Matches the SDK's
        // `SANDBLASTING_RANGE` check.
        let Some((batch_size, end)) =
            scannable_batch_end(base_batch_size, start, range_end, current_tip_height)
        else {
            log::info!(
                "[{}] sync: pending range {} starts after current tip {}, waiting for tip advance",
                elapsed(),
                describe_block_range(range.block_range()),
                current_tip_height,
            );
            break;
        };
        let batch_blocks = u32::from(end).saturating_sub(u32::from(start)) as u64;
        progress_fn(SyncProgressEvent {
            scanned_height: u32::from(start) as u64,
            chain_tip_height: current_tip_height,
        });
        log::info!(
            "[{}] sync: scanning {}-{} (priority {:?}{}, batch={})",
            elapsed(),
            u32::from(start),
            u32::from(end) - 1,
            range.priority(),
            if is_verify_phase {
                ", verify phase"
            } else {
                ""
            },
            batch_size,
        );

        // Download blocks into memory — or use the prefetched data
        // from the previous iteration if it matches this batch.
        let (block_source, prefetched_tree_state) = if let Some(mut pf) = prefetch.take() {
            if pf.start == start && pf.end == end {
                // Prefetch matches. Take the handle out of the
                // Option so Drop doesn't abort a completed task.
                let handle = pf.handle.take().expect("prefetch handle present");
                match handle.await {
                    Ok(Ok((bs, ts))) => {
                        log::debug!(
                            "[{}] sync: using prefetched blocks for {}-{}",
                            elapsed(),
                            u32::from(start),
                            u32::from(end) - 1,
                        );
                        (bs, Some(ts))
                    }
                    _ => {
                        // Prefetch failed — download synchronously.
                        log::warn!("[{}] sync: prefetch failed, downloading fresh", elapsed(),);
                        (download_blocks(&mut client, start, end - 1).await?, None)
                    }
                }
            } else {
                // Range changed (reorg, priority switch, etc.) —
                // Drop the Prefetch, which aborts the background task.
                drop(pf);
                (download_blocks(&mut client, start, end - 1).await?, None)
            }
        } else {
            (download_blocks(&mut client, start, end - 1).await?, None)
        };

        if !block_source.contains_exact_range(u32::from(start), u32::from(end)) {
            return Err(SyncError::other(format!(
                "downloaded compact blocks do not exactly cover {}..{}",
                u32::from(start),
                u32::from(end),
            )));
        }

        // Prefetch: if the current range still has blocks beyond
        // `end`, kick off a background download of the next batch
        // now, so it runs while this batch is scanned, enhanced and
        // resubmitted. The download runs on a cloned gRPC client so
        // it doesn't conflict with the main client's unary RPCs
        // (tree_state, get_latest_block, etc.).
        //
        // When the range is exhausted (end == range.end), we skip
        // the prefetch — the next range comes from
        // suggest_scan_ranges() which needs the DB state the current
        // scan just committed, and we can't predict it in advance.
        if end < range_end {
            let pf_start = end;
            // Recompute batch_size for the prefetch range in case
            // it crosses a sandblasting boundary differently.
            if let Some((_, pf_end)) =
                scannable_batch_end(base_batch_size, pf_start, range_end, current_tip_height)
            {
                let mut pf_client = client.clone();
                prefetch = Some(Prefetch {
                    start: pf_start,
                    end: pf_end,
                    handle: Some(tokio::spawn(async move {
                        let blocks = download_blocks(&mut pf_client, pf_start, pf_end - 1).await?;
                        let tree_state =
                            get_tree_state(&mut pf_client, u32::from(pf_start - 1) as u64).await?;
                        Ok((blocks, tree_state))
                    })),
                });
            } else {
                log::debug!(
                    "[{}] sync: skipping prefetch from {} past current tip {}",
                    elapsed(),
                    u32::from(pf_start),
                    current_tip_height,
                );
            }
        }

        // Fetch and validate the frontier before projecting the checkpoints
        // this batch will create. `update_tree` inserts this exact height as a
        // checkpoint before any compact-block commitments.
        let from_state = if let Some(ts) = prefetched_tree_state {
            ts.to_chain_state()
                .map_err(|e| SyncError::parse(format!("parse tree state: {e}")))?
        } else if should_use_empty_chain_state(&network, start)? {
            chain::ChainState::empty(start - 1, BlockHash([0u8; 32]))
        } else {
            let ts = get_tree_state(&mut client, u32::from(start - 1) as u64).await?;
            ts.to_chain_state()
                .map_err(|e| SyncError::parse(format!("parse tree state: {e}")))?
        };
        if u32::from(from_state.block_height()) != frontier_height {
            return Err(SyncError::other(format!(
                "downloaded tree state height {} does not match scan frontier {frontier_height}",
                u32::from(from_state.block_height()),
            )));
        }

        // Fetch the tip for the post-batch resubmit pass while the scan runs.
        let mut tip_client = client.clone();
        let tip_fetch = tokio::spawn(async move { get_latest_block(&mut tip_client).await });

        // Scan from memory. There are three reorg-adjacent signals from
        // librustzcash that all need to land on `SyncError::Continuity`
        // so the rewind recovery below fires:
        //
        //   - `ChainError::Scan(ScanError::PrevHashMismatch)` / `Scan(
        //     ScanError::BlockHeightDiscontinuity)` — the compact blocks
        //     we just downloaded don't chain to what we scanned last
        //     time. Detected via `is_continuity_error()`.
        //
        //   - `ChainError::Wallet(SqliteClientError::BlockConflict(h))` —
        //     `put_blocks` found an existing row for block `h` with a
        //     different hash. Per librustzcash: "indicates that a
        //     required rewind was not performed". Semantically identical
        //     to a continuity error and equally recoverable via
        //     `truncate_to_height`, so it gets the same treatment.
        //
        // Any other `ChainError::Wallet(e)` is a real DB failure and
        // becomes `SyncError::Db` (Fatal). Everything else (non-scan,
        // non-wallet — e.g. block-source errors, unrecognised scan
        // variants) becomes `SyncError::Other` (retry-with-backoff).
        let scan_result = with_wallet_db_write_lock("sync_engine.retain_and_scan_blocks", || {
            scan_cached_blocks(
                &network,
                &block_source,
                &mut db,
                start,
                &from_state,
                batch_blocks as usize,
            )
            .map_err(|e| match e {
                ChainError::Scan(scan_err) if scan_err.is_continuity_error() => {
                    let at_height = u32::from(scan_err.at_height()) as u64;
                    SyncError::continuity(at_height, scan_err.to_string())
                }
                ChainError::Wallet(SqliteClientError::BlockConflict(at)) => {
                    let at_height = u32::from(at) as u64;
                    SyncError::continuity(
                        at_height,
                        format!("BlockConflict at {at_height}: wallet rewind required"),
                    )
                }
                ChainError::Wallet(ref wallet_err)
                    if matches!(wallet_err, SqliteClientError::NonSequentialBlocks) =>
                {
                    let at_height = u32::from(start) as u64;
                    SyncError::continuity(
                        at_height,
                        format!(
                            "NonSequentialBlocks at {at_height}: from_state does not match first block, rewind required"
                        ),
                    )
                }
                ChainError::Wallet(wallet_err) if is_commitment_tree_root_conflict(&wallet_err) => {
                    let at_height = u32::from(start) as u64;
                    SyncError::continuity(
                        at_height,
                        format!(
                            "commitment tree root conflict while scanning from {at_height}: {wallet_err}"
                        ),
                    )
                }
                ChainError::Wallet(wallet_err) => {
                    // Transient SQLite lock contention (e.g. another wallet
                    // connection holds a write lock) must retry, not bail out.
                    // Everything else is treated as genuine DB failure and
                    // goes Fatal via the per-category retry policy.
                    if is_sqlite_lock_contention(&wallet_err) {
                        SyncError::other(format!("scan: SQLite lock contention: {wallet_err}"))
                    } else {
                        SyncError::db(format!("scan wallet: {wallet_err}"))
                    }
                }
                other => SyncError::other(format!("scan: {other}")),
            })
        });

        // Handle the scan result. On a reorg we rewind the wallet to
        // `at_height - REWIND_DISTANCE` (bounded by `truncate_to_height`'s
        // nearest checkpoint) and restart the scan loop. librustzcash's
        // `suggest_scan_ranges` produces a fresh range list after the
        // truncate, so a `continue` is enough — no manual bookkeeping.
        //
        // Rewind budget is phase-scoped: verify-phase rewinds and
        // main-phase rewinds each have their own cap of
        // `MAX_REWINDS_PER_RUN`. A verify range that keeps flapping won't
        // exhaust the budget the main scan needs to handle an unrelated
        // later reorg.
        match scan_result {
            Ok(_) => {}
            Err(sync_err) => match sync_err.recovery_strategy() {
                RecoveryStrategy::Rewind { to_height } => {
                    let (phase_name, current_rewinds) = if is_verify_phase {
                        ("verify", &mut verify_rewinds_this_run)
                    } else {
                        ("main", &mut main_rewinds_this_run)
                    };
                    if *current_rewinds >= MAX_REWINDS_PER_RUN {
                        log::error!(
                            "[{}] sync: {phase_name} rewind budget exhausted \
                             ({}/{}); propagating error",
                            elapsed(),
                            *current_rewinds,
                            MAX_REWINDS_PER_RUN,
                        );
                        return Err(sync_err);
                    }
                    let rewind_attempt_index = *current_rewinds;
                    let rewind_distance =
                        sync_err.rewind_distance_for_attempt(rewind_attempt_index);
                    let requested_rewind_height = sync_err
                        .rewind_target_for_attempt(rewind_attempt_index)
                        .unwrap_or(to_height);
                    *current_rewinds += 1;
                    // `truncate_to_height` does NOT silently clamp to the
                    // nearest checkpoint. If the requested height is below
                    // the earliest available checkpoint it returns
                    // `SqliteClientError::RequestedRewindInvalid` with
                    // `safe_rewind_height: Option<BlockHeight>`. When
                    // `safe_rewind_height` is `Some(h)` the library is
                    // telling us the deepest checkpoint it can land on;
                    // retry at that height so a reorg near genesis (or
                    // right after a birthday-bounded import) still
                    // recovers. When it's `None` there is genuinely
                    // nowhere safe to rewind to, and we surface the
                    // failure as fatal.
                    let target = BlockHeight::from_u32(requested_rewind_height as u32);
                    let actual_rewind_height = with_wallet_db_write_lock(
                        "sync_engine.truncate_to_height",
                        || -> Result<BlockHeight, SyncError> {
                            match db.truncate_to_height(target) {
                                Ok(h) => Ok(h),
                                Err(SqliteClientError::RequestedRewindInvalid {
                                    safe_rewind_height: Some(safe),
                                    requested_height,
                                }) => {
                                    log::warn!(
                                        "[{}] sync: {phase_name} rewind target {requested_height} \
                                         below earliest checkpoint; retrying at safe_rewind_height={safe}",
                                        elapsed(),
                                    );
                                    db.truncate_to_height(safe).map_err(|e| {
                                        if is_sqlite_lock_contention(&e) {
                                            SyncError::other(format!(
                                                "truncate_to_height({safe}) retry: SQLite lock contention: {e}"
                                            ))
                                        } else {
                                            SyncError::db(format!(
                                                "truncate_to_height({safe}) retry after RequestedRewindInvalid: {e}"
                                            ))
                                        }
                                    })
                                }
                                Err(SqliteClientError::RequestedRewindInvalid {
                                    safe_rewind_height: None,
                                    requested_height,
                                }) => {
                                    log::error!(
                                        "[{}] sync: {phase_name} rewind to {requested_height} \
                                         rejected and no safe_rewind_height is available; \
                                         cannot recover from this reorg in-place",
                                        elapsed(),
                                    );
                                    Err(SyncError::db(format!(
                                        "truncate_to_height({requested_height}): no safe rewind height"
                                    )))
                                }
                                Err(e) if is_sqlite_lock_contention(&e) => {
                                    // Transient lock contention on the rewind. The
                                    // outer retry wrapper will re-invoke run_sync_impl
                                    // after a backoff, which re-detects the continuity
                                    // error and triggers the rewind again. If the
                                    // lock has cleared by then, the retry succeeds.
                                    Err(SyncError::other(format!(
                                        "truncate_to_height({requested_rewind_height}): SQLite lock contention: {e}"
                                    )))
                                }
                                Err(e) => Err(SyncError::db(format!(
                                    "truncate_to_height({requested_rewind_height}): {e}"
                                ))),
                            }
                        },
                    )?;
                    let current_tip = BlockHeight::from_u32(current_tip_height as u32);
                    let post_rewind_ranges = with_wallet_db_write_lock(
                        "sync_engine.update_chain_tip.after_rewind",
                        || -> Result<Vec<ScanRange>, SyncError> {
                            db.update_chain_tip(current_tip).map_err(|e| {
                                SyncError::db(format!(
                                    "update_chain_tip({current_tip_height}) after rewind: {e}"
                                ))
                            })?;
                            db.suggest_scan_ranges().map_err(|e| {
                                SyncError::db(format!("suggest_scan_ranges after rewind: {e}"))
                            })
                        },
                    )?;
                    let post_rewind_pending = pending_scan_blocks(&post_rewind_ranges);
                    let first_pending = first_pending_scan_range(&post_rewind_ranges)
                        .unwrap_or_else(|| "none".into());
                    let summary = sync::wallet_scan_heights(&mut db).map_err(SyncError::db)?;
                    let actual_rewind_height_u64 = u32::from(actual_rewind_height) as u64;
                    log::info!(
                        "[{}] sync: {phase_name} rewound to {actual_rewind_height} \
                         after reorg (requested={requested_rewind_height}, \
                         distance={rewind_distance}, attempt {}/{}); \
                         post_rewind_pending={post_rewind_pending}, first_pending={first_pending}, \
                         summary={summary:?}; restarting scan loop",
                        elapsed(),
                        *current_rewinds,
                        MAX_REWINDS_PER_RUN,
                    );
                    force_witness_check_this_run = true;
                    if actual_rewind_height_u64 < current_tip_height && post_rewind_pending == 0 {
                        return Err(SyncError::continuity(
                            current_tip_height,
                            format!(
                                "post-rewind scan queue empty after rewinding to \
                                 {actual_rewind_height_u64}, but lightwalletd tip is \
                                 {current_tip_height}"
                            ),
                        ));
                    }
                    prefetch = None;
                    continue;
                }
                RecoveryStrategy::RetryWithBackoff | RecoveryStrategy::Fatal => {
                    return Err(sync_err);
                }
            },
        };

        // Truncation can temporarily clear a transaction's mined height while
        // retaining note positions that prove compact scanning found it mined.
        // Exclude those transactions from recovery resubmission while scanning
        // can still restore their mined heights.
        let post_scan_ranges = db
            .suggest_scan_ranges()
            .map_err(|e| SyncError::db(format!("suggest_scan_ranges: {e}")))?;
        let resubmit_exclusions = recovery_resubmit_exclusions(db_data_path, &post_scan_ranges)?;

        // Enhancement
        run_enhancement(&mut client, &mut db, db_data_path, network).await?;

        // Post-batch auto-resubmit. Matches zcash-android-wallet-sdk's
        // lines 593/701 call sites (end of verify batch / end of
        // regular batch).
        //
        // We deliberately re-fetch the chain tip via
        // `get_latest_block` before each pass instead of reusing
        // `tip.height` captured once at the top of `run_sync_impl`.
        // `get_resubmittable_txs` decides "still inside expiry
        // window" with `expiry_height > current_height`; using the
        // stale top-of-sync tip meant a long catch-up session
        // (several thousand blocks) could keep rebroadcasting txs
        // whose expiry had already passed against the real chain
        // tip. Refreshing here is one extra unary gRPC per batch,
        // which is cheap compared to the batch download itself and
        // closes the "resubmit expired tx forever" regression
        // caught by Codex 2nd-round review finding 2. The request
        // starts before the scan, so the tip is at most one scan
        // and enhancement old.
        //
        // Best-effort: helper swallows per-tx failures, we ignore
        // the return value, and if the tip refresh itself fails we
        // log and skip the pass rather than falling back to the
        // stale height (the whole point of the refresh is to avoid
        // rebroadcasting against a stale expiry window).
        match tip_fetch
            .await
            .map_err(|e| SyncError::other(format!("tip fetch task: {e}")))
            .and_then(|tip| tip)
            .map(|tip| tip.height as u32)
        {
            Ok(fresh_tip_height) => {
                // Promote the fresh tip to the authoritative value
                // so progress events and the final completion event
                // use the latest chain height, not the one from
                // sync startup. Also update the DB so
                // suggest_scan_ranges picks up any new blocks that
                // appeared since the previous tip fetch.
                //
                // IMPORTANT: update_chain_tip MUST succeed before
                // we bump current_tip_height. If the DB write fails,
                // suggest_scan_ranges still operates on the old tip
                // and the loop may break with isComplete=true —
                // bumping current_tip_height prematurely would make
                // the completion event claim a height the wallet
                // never actually scanned. (Codex 3rd-round finding.)
                if (fresh_tip_height as u64) > current_tip_height {
                    let fresh_bh = BlockHeight::from_u32(fresh_tip_height);
                    match with_wallet_db_write_lock(
                        "sync_engine.update_chain_tip.post_batch",
                        || db.update_chain_tip(fresh_bh),
                    ) {
                        Ok(_) => {
                            current_tip_height = fresh_tip_height as u64;
                        }
                        Err(e) => {
                            log::warn!(
                                "[{}] sync: post-batch update_chain_tip({fresh_tip_height}) \
                                 failed, keeping tip at {current_tip_height}: {e}",
                                elapsed(),
                            );
                        }
                    }
                }
                let _ = crate::sync::resubmit_pending_transactions(
                    db_data_path,
                    &mut client,
                    fresh_tip_height,
                    &resubmit_exclusions,
                )
                .await;
            }
            Err(e) => {
                log::warn!(
                    "[{}] sync: resubmit tip refresh failed, skipping pass: {e}",
                    elapsed(),
                );
            }
        }
        // Report progress
        let remaining: u64 = post_scan_ranges
            .iter()
            .filter(|r| is_pending_scan_range(r))
            .map(|r| {
                u32::from(r.block_range().end).saturating_sub(u32::from(r.block_range().start))
                    as u64
            })
            .sum();
        let progress = SyncProgressEvent {
            scanned_height: u32::from(end) as u64,
            chain_tip_height: current_tip_height,
        };
        log::info!("[{}] sync: {} blocks remaining", elapsed(), remaining);
        progress_fn(progress);
        #[cfg(debug_assertions)]
        maybe_sleep_for_e2e_sync_batch_delay().await;
    }

    let (final_scanned_height, final_tip_height) =
        ensure_complete_scan_state(&mut db, current_tip_height)?;
    log::info!(
        "[{}] sync: completed (fully_scanned={}, chain_tip={})",
        elapsed(),
        final_scanned_height,
        final_tip_height,
    );
    with_wallet_db_write_lock("sync_engine.mark_sync_completed", || {
        mark_sync_completed(db_data_path, final_tip_height)
    })
    .map_err(SyncError::db)?;
    // Final progress
    let final_progress = SyncProgressEvent {
        scanned_height: final_scanned_height,
        chain_tip_height: final_tip_height,
    };
    progress_fn(final_progress);

    Ok(())
}

// ==================== Helpers ====================

fn open_db(path: &str, network: WalletNetwork) -> Result<WalletDatabase, SyncError> {
    open_wallet_db_with_timeout(path, network, SYNC_DB_BUSY_TIMEOUT)
        .map_err(|e| SyncError::db(format!("DB open: {e}")))
}

/// Returns `true` when `err` wraps a transient SQLite lock-contention
/// primary code (`SQLITE_BUSY` or `SQLITE_LOCKED`). These are not
/// corruption — they fire when another connection currently holds a
/// write lock on the wallet DB. The wallet opens separate connections
/// for balance queries, the send flow, and the sync loop itself, so
/// this condition is reachable in normal operation and must be
/// classified as transient (retry-with-backoff) rather than fatal.
///
/// Extended codes (`SQLITE_BUSY_RECOVERY`, `SQLITE_BUSY_SNAPSHOT`,
/// `SQLITE_BUSY_TIMEOUT`, `SQLITE_LOCKED_SHAREDCACHE`,
/// `SQLITE_LOCKED_VTAB`) are all rolled up into the two primary codes
/// by `rusqlite`, so matching on `ErrorCode::DatabaseBusy` /
/// `DatabaseLocked` catches all of them.
fn is_sqlite_lock_contention(err: &SqliteClientError) -> bool {
    if let SqliteClientError::DbError(rusqlite::Error::SqliteFailure(inner, _)) = err {
        matches!(
            inner.code,
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked,
        )
    } else {
        false
    }
}

fn is_commitment_tree_root_conflict(err: &SqliteClientError) -> bool {
    matches!(
        err,
        SqliteClientError::CommitmentTree(ShardTreeError::Insert(InsertionError::Conflict(_)))
    )
}

fn is_witness_position_beyond_tree(err: &SqliteClientError) -> bool {
    matches!(
        err,
        SqliteClientError::CommitmentTree(ShardTreeError::Query(QueryError::NotContained(_)))
    )
}

fn clear_unmined_note_commitment_positions(db_data_path: &str) -> Result<usize, SyncError> {
    let mut conn = open_wallet_raw_conn_with_timeout(db_data_path, SYNC_DB_BUSY_TIMEOUT)
        .map_err(|e| SyncError::db(format!("open DB to repair unmined note positions: {e}")))?;
    let tx = conn
        .transaction()
        .map_err(|e| SyncError::db(format!("begin unmined note position repair: {e}")))?;
    let mut cleared = 0;

    for table in [
        "sapling_received_notes",
        "orchard_received_notes",
        "ironwood_received_notes",
    ] {
        let exists = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
                [table],
                |row| row.get::<_, bool>(0),
            )
            .map_err(|e| SyncError::db(format!("inspect {table} before position repair: {e}")))?;
        if !exists {
            continue;
        }

        cleared += tx
            .execute(
                &format!(
                    "UPDATE {table} AS rn
                     SET commitment_tree_position = NULL
                     WHERE commitment_tree_position IS NOT NULL
                     AND EXISTS (
                         SELECT 1 FROM transactions AS tx
                         WHERE tx.id_tx = rn.transaction_id
                         AND tx.mined_height IS NULL
                     )"
                ),
                [],
            )
            .map_err(|e| SyncError::db(format!("repair unmined positions in {table}: {e}")))?;
    }

    tx.commit()
        .map_err(|e| SyncError::db(format!("commit unmined note position repair: {e}")))?;
    Ok(cleared)
}

fn should_use_empty_chain_state(
    network: &WalletNetwork,
    start: BlockHeight,
) -> Result<bool, SyncError> {
    let sapling_activation_height = network
        .activation_height(NetworkUpgrade::Sapling)
        .ok_or_else(|| SyncError::parse("Sapling activation height is unavailable"))?;
    Ok(start <= sapling_activation_height)
}

// ======================== Public Lightwalletd Helpers ========================

/// Get the chain tip from a lightwalletd endpoint, bounded by a timeout so
/// a dead endpoint cannot hang a one-shot command. Used by `status`.
pub fn fetch_chain_tip(lightwalletd_url: &str, network: WalletNetwork) -> Result<u64, String> {
    let rt = tokio::runtime::Runtime::new().map_err(|e| format!("tokio: {e}"))?;
    rt.block_on(async {
        let mut client = connect(network, lightwalletd_url)
            .await
            .map_err(|e| e.to_string())?;
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            crate::sync_engine::lwd::get_latest_block(&mut client),
        )
        .await
        .map_err(|_| "get_latest_block: timed out".to_string())?
        .map(|tip| tip.height)
        .map_err(|e| e.to_string())
    })
}

/// The birthday of an account whose first block is `height`: empty note
/// commitment trees at or below Sapling activation, otherwise the tree state
/// a lightwalletd endpoint reports for block `height - 1`.
pub fn fetch_birthday(
    lightwalletd_url: &str,
    network: WalletNetwork,
    height: u64,
) -> Result<AccountBirthday, String> {
    let start = BlockHeight::try_from(height)
        .map_err(|_| format!("Birthday {height} is not a valid block height"))?;
    if should_use_empty_chain_state(&network, start).map_err(|e| e.to_string())? {
        let prior = chain::ChainState::empty(start - 1, BlockHash([0u8; 32]));
        return Ok(AccountBirthday::from_parts(prior, None));
    }
    let rt = tokio::runtime::Runtime::new().map_err(|e| format!("tokio: {e}"))?;
    let tree_state = rt.block_on(async {
        let mut client = connect(network, lightwalletd_url)
            .await
            .map_err(|e| e.to_string())?;
        get_tree_state(&mut client, height - 1)
            .await
            .map_err(|e| e.to_string())
    })?;
    if tree_state.height != height - 1 {
        return Err(format!(
            "lightwalletd returned the tree state for height {}, not {}",
            tree_state.height,
            height - 1
        ));
    }
    AccountBirthday::from_treestate(tree_state, None)
        .map_err(|e| format!("Could not read the tree state before birthday {height}: {e}"))
}

// ==================== Tests ====================
//
// Error-taxonomy tests now live alongside their types in `error.rs`. The
// only test that has to stay here is `sqlite_lock_contention_is_recognised`,
// because it exercises the `is_sqlite_lock_contention` helper that still
// lives in this module. A follow-up refactor commit moves the helper (and
// this test) into the lwd submodule.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_chain_state_uses_network_activation_height() {
        assert!(
            should_use_empty_chain_state(&WalletNetwork::Main, BlockHeight::from_u32(419_200))
                .unwrap()
        );
        assert!(!should_use_empty_chain_state(
            &WalletNetwork::Main,
            BlockHeight::from_u32(419_201)
        )
        .unwrap());

        assert!(
            should_use_empty_chain_state(&WalletNetwork::Regtest, BlockHeight::from_u32(1))
                .unwrap()
        );
        assert!(
            !should_use_empty_chain_state(&WalletNetwork::Regtest, BlockHeight::from_u32(141))
                .unwrap()
        );
    }

    #[test]
    fn scannable_batch_end_clamps_to_current_tip() {
        assert_eq!(
            scannable_batch_end(
                2_000,
                BlockHeight::from_u32(121),
                BlockHeight::from_u32(131),
                121,
            ),
            Some((2_000, BlockHeight::from_u32(122))),
        );
    }

    #[test]
    fn scannable_batch_end_skips_ranges_past_current_tip() {
        assert_eq!(
            scannable_batch_end(
                2_000,
                BlockHeight::from_u32(122),
                BlockHeight::from_u32(131),
                121,
            ),
            None,
        );
    }

    #[test]
    fn batches_before_sandblasting_end_at_its_start() {
        assert_eq!(
            batch_size_for_range(2_000, BlockHeight::from_u32(1_300_000)),
            2_000
        );
        assert_eq!(
            batch_size_for_range(2_000, BlockHeight::from_u32(1_709_000)),
            1_000
        );
    }

    #[test]
    fn batches_inside_sandblasting_use_the_small_size() {
        assert_eq!(
            batch_size_for_range(2_000, BlockHeight::from_u32(SANDBLASTING_START)),
            BATCH_SIZE_SANDBLASTING
        );
        assert_eq!(
            batch_size_for_range(2_000, BlockHeight::from_u32(SANDBLASTING_END - 1)),
            BATCH_SIZE_SANDBLASTING
        );
    }

    #[test]
    fn batches_after_sandblasting_use_the_base_size() {
        assert_eq!(
            batch_size_for_range(2_000, BlockHeight::from_u32(SANDBLASTING_END)),
            2_000
        );
    }

    #[test]
    fn witness_check_runs_without_a_clean_marker() {
        assert_eq!(
            decide_witness_check(WitnessCheckMeta::default(), 3_364_776, false),
            WitnessCheckDecision::Run(WitnessCheckRunReason::MissingMarker),
        );
    }

    #[test]
    fn witness_check_skips_when_recent_clean_marker_is_fresh() {
        let meta = WitnessCheckMeta {
            last_clean_height: Some(3_364_774),
        };

        assert_eq!(
            decide_witness_check(meta, 3_364_776, false),
            WitnessCheckDecision::Skip {
                last_clean_height: 3_364_774,
                age_blocks: 2,
            },
        );
    }

    #[test]
    fn witness_check_runs_when_forced_or_marker_is_stale() {
        let meta = WitnessCheckMeta {
            last_clean_height: Some(1_000),
        };

        assert_eq!(
            decide_witness_check(meta, 1_001, true),
            WitnessCheckDecision::Run(WitnessCheckRunReason::Forced),
        );
        assert_eq!(
            decide_witness_check(meta, 1_000 + WITNESS_CHECK_MAX_CLEAN_AGE_BLOCKS, false),
            WitnessCheckDecision::Run(WitnessCheckRunReason::MaxCleanAgeReached {
                age_blocks: WITNESS_CHECK_MAX_CLEAN_AGE_BLOCKS,
            }),
        );
    }

    #[test]
    fn witness_check_runs_when_tip_rewinds() {
        assert_eq!(
            decide_witness_check(
                WitnessCheckMeta {
                    last_clean_height: Some(3_364_776),
                },
                3_364_775,
                false,
            ),
            WitnessCheckDecision::Run(WitnessCheckRunReason::TipBelowLastClean {
                last_clean_height: 3_364_776,
            }),
        );
    }

    #[test]
    fn witness_check_clean_marker_round_trips_through_sync_meta_table() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let db_path = file.path().to_str().unwrap();

        mark_witness_check_clean(db_path, 3_364_776).unwrap();

        assert_eq!(
            read_witness_check_meta(db_path).unwrap(),
            WitnessCheckMeta {
                last_clean_height: Some(3_364_776),
            },
        );
    }

    #[test]
    fn completed_sync_marker_round_trips_and_advances() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let db_path = file.path().to_str().unwrap();

        assert_eq!(read_sync_completion_meta(db_path).unwrap(), (None, None));
        mark_sync_completed(db_path, 3_364_776).unwrap();
        assert_eq!(
            read_sync_completion_meta(db_path).unwrap(),
            (Some(3_364_776), Some(false)),
        );
        let conn = rusqlite::Connection::open(db_path).unwrap();
        let stored_keys: Vec<String> = conn
            .prepare("SELECT key FROM ext_zorg_sync_meta ORDER BY key")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            stored_keys,
            [
                "last_completed_sync_at",
                "last_completed_sync_height",
                "sync_in_progress"
            ]
        );
        drop(conn);
        mark_sync_started(db_path).unwrap();
        assert_eq!(
            completed_sync_height_for_status(db_path, 3_364_776, 3_364_776).unwrap(),
            None,
        );
        mark_sync_completed(db_path, 3_364_777).unwrap();
        assert_eq!(
            read_sync_completion_meta(db_path).unwrap(),
            (Some(3_364_777), Some(false)),
        );
    }

    #[test]
    fn completion_status_uses_tip_for_legacy_wallets_without_markers() {
        let legacy_file = tempfile::NamedTempFile::new().unwrap();
        let legacy_path = legacy_file.path().to_str().unwrap();
        assert_eq!(
            completed_sync_height_for_status(legacy_path, 100, 100).unwrap(),
            Some(100),
        );
        assert_eq!(
            read_sync_completion_meta(legacy_path).unwrap(),
            (None, None),
        );

        let active_sync_file = tempfile::NamedTempFile::new().unwrap();
        let active_sync_path = active_sync_file.path().to_str().unwrap();
        mark_sync_completed(active_sync_path, 100).unwrap();
        mark_sync_started(active_sync_path).unwrap();
        assert_eq!(
            completed_sync_height_for_status(active_sync_path, 100, 100).unwrap(),
            None,
        );
        assert_eq!(
            read_sync_completion_meta(active_sync_path).unwrap(),
            (Some(100), Some(true)),
        );
    }

    #[test]
    fn sqlite_lock_contention_is_recognised() {
        use rusqlite::ffi;

        // DatabaseBusy → transient
        let busy = SqliteClientError::DbError(rusqlite::Error::SqliteFailure(
            ffi::Error::new(ffi::SQLITE_BUSY),
            Some("database is locked".into()),
        ));
        assert!(is_sqlite_lock_contention(&busy));

        // DatabaseLocked → transient
        let locked = SqliteClientError::DbError(rusqlite::Error::SqliteFailure(
            ffi::Error::new(ffi::SQLITE_LOCKED),
            Some("database table is locked".into()),
        ));
        assert!(is_sqlite_lock_contention(&locked));

        // SQLITE_CORRUPT → NOT transient (genuine DB failure)
        let corrupt = SqliteClientError::DbError(rusqlite::Error::SqliteFailure(
            ffi::Error::new(ffi::SQLITE_CORRUPT),
            None,
        ));
        assert!(!is_sqlite_lock_contention(&corrupt));

        // SQLITE_IOERR → NOT transient under our policy (could be
        // transient in principle but not covered by this helper). Kept
        // as-is so a future expansion to include IOERR_* codes is a
        // deliberate change.
        let ioerr = SqliteClientError::DbError(rusqlite::Error::SqliteFailure(
            ffi::Error::new(ffi::SQLITE_IOERR),
            None,
        ));
        assert!(!is_sqlite_lock_contention(&ioerr));

        // A non-DbError wallet variant is trivially not lock contention.
        let block_conflict = SqliteClientError::BlockConflict(
            zcash_protocol::consensus::BlockHeight::from_u32(2_500_000),
        );
        assert!(!is_sqlite_lock_contention(&block_conflict));
    }

    #[test]
    fn commitment_tree_root_conflict_is_recognised() {
        use incrementalmerkletree::{Address, Level};

        let conflict = SqliteClientError::CommitmentTree(ShardTreeError::Insert(
            InsertionError::Conflict(Address::from_parts(Level::new(7), 391_096)),
        ));
        assert!(is_commitment_tree_root_conflict(&conflict));

        let out_of_range =
            SqliteClientError::CommitmentTree(ShardTreeError::Insert(InsertionError::OutOfRange(
                incrementalmerkletree::Position::from(0),
                incrementalmerkletree::Position::from(1)..incrementalmerkletree::Position::from(2),
            )));
        assert!(!is_commitment_tree_root_conflict(&out_of_range));

        let block_conflict = SqliteClientError::BlockConflict(
            zcash_protocol::consensus::BlockHeight::from_u32(2_500_000),
        );
        assert!(!is_commitment_tree_root_conflict(&block_conflict));
    }

    #[test]
    fn witness_position_beyond_tree_is_recognised() {
        use incrementalmerkletree::{Address, Level};

        let not_contained = SqliteClientError::CommitmentTree(ShardTreeError::Query(
            QueryError::NotContained(Address::from_parts(Level::new(0), 0)),
        ));
        assert!(is_witness_position_beyond_tree(&not_contained));

        let incomplete = SqliteClientError::CommitmentTree(ShardTreeError::Query(
            QueryError::TreeIncomplete(vec![]),
        ));
        assert!(!is_witness_position_beyond_tree(&incomplete));
    }

    #[test]
    fn unmined_note_position_repair_preserves_mined_notes() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let db_path = file.path().to_str().unwrap();
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE transactions (
                id_tx INTEGER PRIMARY KEY,
                mined_height INTEGER
             );
             CREATE TABLE sapling_received_notes (
                id INTEGER PRIMARY KEY,
                transaction_id INTEGER NOT NULL,
                commitment_tree_position INTEGER
             );
             CREATE TABLE ironwood_received_notes (
                id INTEGER PRIMARY KEY,
                transaction_id INTEGER NOT NULL,
                commitment_tree_position INTEGER
             );
             INSERT INTO transactions (id_tx, mined_height) VALUES (1, NULL), (2, 500);
             INSERT INTO sapling_received_notes
                (id, transaction_id, commitment_tree_position)
                VALUES (1, 1, 3), (2, 2, 4);
             INSERT INTO ironwood_received_notes
                (id, transaction_id, commitment_tree_position)
                VALUES (1, 1, 0), (2, 2, 1);",
        )
        .unwrap();
        drop(conn);

        assert_eq!(clear_unmined_note_commitment_positions(db_path).unwrap(), 2);

        let conn = rusqlite::Connection::open(db_path).unwrap();
        let sapling_positions = conn
            .prepare("SELECT commitment_tree_position FROM sapling_received_notes ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get::<_, Option<u64>>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let ironwood_positions = conn
            .prepare("SELECT commitment_tree_position FROM ironwood_received_notes ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get::<_, Option<u64>>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(sapling_positions, vec![None, Some(4)]);
        assert_eq!(ironwood_positions, vec![None, Some(1)]);
        assert_eq!(clear_unmined_note_commitment_positions(db_path).unwrap(), 0);
    }
}
