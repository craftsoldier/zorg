use std::fmt;

use nonempty::NonEmpty;
use rusqlite::{params, OptionalExtension};
use shardtree::error::{InsertionError, ShardTreeError};
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

use zcash_client_backend::proto::service::compact_tx_streamer_client::CompactTxStreamerClient;

mod block_source;
mod enhance;
mod lwd;
mod transparent;

use enhance::run_enhancement;
pub use lwd::{chain_tip, connect_endpoint, LwdClient};
use lwd::{download_blocks, download_subtree_roots, get_tree_state};
pub(crate) use lwd::{get_latest_block, send_transaction};
use transparent::refresh_utxos;

/// Classified sync-engine failure. Carries enough information for the retry
/// wrapper to pick the right recovery strategy via `recovery_strategy`.
#[derive(Debug, Clone)]
pub(crate) enum SyncError {
    /// `scan_cached_blocks` reported a `PrevHashMismatch` or
    /// `BlockHeightDiscontinuity` at `at_height`, meaning the wallet's
    /// stored chain state no longer agrees with the one lightwalletd is
    /// serving. Recovery is to `truncate_to_height(at_height - REWIND_DISTANCE)`
    /// and restart the scan loop from the rewound state.
    Continuity { at_height: u64, detail: String },

    /// Transient network or gRPC failure. The existing exponential-backoff
    /// retry path is the right response.
    Network(String),

    /// Local SQLite / `WalletDb` failure. Usually non-retryable (permissions,
    /// disk full, schema corruption) — propagate up and let the caller decide.
    Db(String),

    /// Serialization / deserialization failure for a compact block, tree
    /// state, transaction, or similar on-wire structure. Non-retryable
    /// because re-fetching the same bytes is unlikely to parse differently.
    Parse(String),

    /// The server serves a different network than the wallet. Another
    /// attempt reaches the same server, so it is not retried.
    WrongNetwork(String),

    /// Unclassified error. Treated as transient by default (retry-with-backoff)
    /// so we fail safe toward "try again" rather than "bail out".
    Other(String),
}

/// Strategy the sync loop should apply when it encounters a `SyncError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecoveryStrategy {
    /// Retry the failing operation after exponential backoff. The outer
    /// `run_sync_inner` wrapper already implements this with a 3-retry,
    /// 2s/4s/8s schedule.
    RetryWithBackoff,

    /// Rewind the wallet's chain state to `to_height` (via
    /// `WalletDb::truncate_to_height`) and restart the scan loop. Used for
    /// continuity errors.
    Rewind { to_height: u64 },

    /// No automatic recovery is safe. Propagate the error up to the caller.
    Fatal,
}

/// Number of blocks the wallet rewinds past a detected reorg. Matches
/// `CompactBlockProcessor.REWIND_DISTANCE` in zcash-android-wallet-sdk.
///
/// The actual rewind height may land earlier than `at_height - REWIND_DISTANCE`
/// because `truncate_to_height` only accepts checkpoint boundaries; that's
/// librustzcash's responsibility to enforce.
pub(crate) const REWIND_DISTANCE: u64 = 10;

/// Commitment-tree root conflicts can be produced by a stale local tree over a
/// wider range than a normal one-block chain reorg. Escalate each retry inside
/// a single sync run so we do not keep landing on the same bad shard boundary.
pub(crate) const COMMITMENT_TREE_REWIND_DISTANCES: [u64; 3] = [10, 100, 1000];

/// Maximum number of reorg-triggered rewinds allowed inside a single
/// `run_sync_impl` invocation. Caps runaway rewind loops: if the chain is
/// flapping fast enough to blow through this budget, `run_sync_impl` bails
/// out so the outer `run_sync_inner` retry wrapper (and eventually the
/// caller) can try again with a fresh budget. Without this the same
/// sync run could keep rewinding backward indefinitely.
pub(crate) const MAX_REWINDS_PER_RUN: u32 = 3;

impl SyncError {
    /// Log and construct a `Continuity` error in one step.
    ///
    /// Uses `log::warn!` rather than `log::error!` because a reorg is an
    /// expected, recoverable event — the reorg-recovery path in the scan
    /// loop will rewind the wallet and restart automatically.
    pub(crate) fn continuity(at_height: u64, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        log::warn!("sync: chain continuity broken at height {at_height}: {detail}");
        SyncError::Continuity { at_height, detail }
    }

    /// Log and construct a `Network` error in one step.
    pub(crate) fn net(msg: impl Into<String>) -> Self {
        let msg = msg.into();
        log::error!("sync: {msg}");
        SyncError::Network(msg)
    }

    /// Log and construct a `Db` error in one step.
    pub(crate) fn db(msg: impl Into<String>) -> Self {
        let msg = msg.into();
        log::error!("sync: {msg}");
        SyncError::Db(msg)
    }

    /// Log and construct a `Parse` error in one step.
    pub(crate) fn parse(msg: impl Into<String>) -> Self {
        let msg = msg.into();
        log::error!("sync: {msg}");
        SyncError::Parse(msg)
    }

    /// Log and construct an `Other` error in one step. Used for call sites
    /// that don't fit a specific category (and for `scan_cached_blocks`
    /// fall-through cases that the scan branch didn't classify).
    pub(crate) fn other(msg: impl Into<String>) -> Self {
        let msg = msg.into();
        log::error!("sync: {msg}");
        SyncError::Other(msg)
    }

    /// Map this error to the recovery action the sync loop should take.
    pub(crate) fn recovery_strategy(&self) -> RecoveryStrategy {
        match self {
            SyncError::Continuity { at_height, .. } => RecoveryStrategy::Rewind {
                to_height: at_height.saturating_sub(REWIND_DISTANCE),
            },
            SyncError::Network(_) | SyncError::Other(_) => RecoveryStrategy::RetryWithBackoff,
            SyncError::Db(_) | SyncError::Parse(_) | SyncError::WrongNetwork(_) => {
                RecoveryStrategy::Fatal
            }
        }
    }

    pub(crate) fn is_commitment_tree_root_conflict(&self) -> bool {
        match self {
            SyncError::Continuity { detail, .. } => detail
                .to_ascii_lowercase()
                .contains("commitment tree root conflict"),
            _ => false,
        }
    }

    pub(crate) fn rewind_distance_for_attempt(&self, attempt_index: u32) -> u64 {
        if self.is_commitment_tree_root_conflict() {
            let idx = usize::try_from(attempt_index).unwrap_or(usize::MAX);
            COMMITMENT_TREE_REWIND_DISTANCES
                .get(idx)
                .copied()
                .unwrap_or_else(|| *COMMITMENT_TREE_REWIND_DISTANCES.last().unwrap())
        } else {
            REWIND_DISTANCE
        }
    }

    pub(crate) fn rewind_target_for_attempt(&self, attempt_index: u32) -> Option<u64> {
        match self {
            SyncError::Continuity { at_height, .. } => {
                Some(at_height.saturating_sub(self.rewind_distance_for_attempt(attempt_index)))
            }
            _ => None,
        }
    }
}

impl fmt::Display for SyncError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SyncError::Continuity { at_height, detail } => {
                write!(f, "chain continuity broken at height {at_height}: {detail}")
            }
            SyncError::Network(msg) => write!(f, "network: {msg}"),
            SyncError::Db(msg) => write!(f, "db: {msg}"),
            SyncError::Parse(msg) => write!(f, "parse: {msg}"),
            SyncError::WrongNetwork(msg) => write!(f, "{msg}"),
            SyncError::Other(msg) => write!(f, "other: {msg}"),
        }
    }
}

impl std::error::Error for SyncError {}

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
const SYNC_META_TABLE: &str = "ext_zorg_sync_meta";
const LAST_COMPLETED_SYNC_HEIGHT_KEY: &str = "last_completed_sync_height";
const SYNC_IN_PROGRESS_KEY: &str = "sync_in_progress";
const LAST_COMPLETED_SYNC_AT_KEY: &str = "last_completed_sync_at";
const WITNESS_CHECK_VERSION_KEY: &str = "witness_check_version";
type SyncCompletionMeta = (Option<u64>, Option<bool>);
// `truncate_to_chain_state` only injects a canonical frontier when the requested
// height is below the retained checkpoint window. Start at the pruning depth
// and escalate so corrupted anchor checkpoints do not survive the repair.
const ANCHOR_ROOT_REPAIR_REWIND_DISTANCES: [u32; 3] = [100, 1000, 10_000];

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

fn pending_scan_blocks(ranges: &[ScanRange]) -> u64 {
    ranges
        .iter()
        .filter(|r| is_pending_scan_range(r))
        .map(|r| block_range_len(r.block_range()))
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

fn read_sync_meta<T>(conn: &rusqlite::Connection, key: &str) -> Result<Option<T>, String>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let table_exists = conn
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1
            )",
            params![SYNC_META_TABLE],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|e| format!("read sync metadata table existence: {e}"))?
        != 0;
    if !table_exists {
        return Ok(None);
    }
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM ext_zorg_sync_meta WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| format!("read sync metadata value {key}: {e}"))?;
    Ok(value.and_then(|value| {
        value
            .parse()
            .map_err(|e| {
                log::warn!("sync: ignoring invalid sync metadata value {key}={value:?}: {e}")
            })
            .ok()
    }))
}

fn read_sync_completion_meta(db_data_path: &str) -> Result<SyncCompletionMeta, String> {
    let conn = open_readonly_conn_with_timeout(db_data_path, Some(SYNC_DB_BUSY_TIMEOUT))?;
    Ok((
        read_sync_meta(&conn, LAST_COMPLETED_SYNC_HEIGHT_KEY)?,
        read_sync_meta::<u32>(&conn, SYNC_IN_PROGRESS_KEY)?.map(|value| value != 0),
    ))
}

fn write_sync_meta(db_data_path: &str, entries: &[(&str, String)]) -> Result<(), String> {
    let mut conn = open_wallet_raw_conn_with_timeout(db_data_path, SYNC_DB_BUSY_TIMEOUT)?;
    ensure_sync_meta_table(&conn)?;
    let tx = conn
        .transaction()
        .map_err(|e| format!("begin sync metadata transaction: {e}"))?;
    for (key, value) in entries {
        tx.execute(
            "INSERT INTO ext_zorg_sync_meta(key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )
        .map_err(|e| format!("write sync metadata {key}: {e}"))?;
    }
    tx.commit()
        .map_err(|e| format!("commit sync metadata transaction: {e}"))
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

fn mark_witness_check_clean(db_data_path: &str) -> Result<(), String> {
    write_sync_meta(
        db_data_path,
        &[(WITNESS_CHECK_VERSION_KEY, env!("CARGO_PKG_VERSION").into())],
    )
}

pub(crate) fn completed_sync_height_for_status(db_data_path: &str) -> Result<Option<u64>, String> {
    let (completed_height, in_progress) = read_sync_completion_meta(db_data_path)?;
    Ok(completed_height.filter(|_| in_progress != Some(true)))
}

fn mark_sync_started(db_data_path: &str) -> Result<(), String> {
    write_sync_meta(db_data_path, &[(SYNC_IN_PROGRESS_KEY, "1".into())])
}

fn mark_sync_completed(db_data_path: &str, completed_tip_height: u64) -> Result<(), String> {
    let completed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    write_sync_meta(
        db_data_path,
        &[
            (
                LAST_COMPLETED_SYNC_HEIGHT_KEY,
                completed_tip_height.to_string(),
            ),
            (SYNC_IN_PROGRESS_KEY, "0".into()),
            (LAST_COMPLETED_SYNC_AT_KEY, completed_at.to_string()),
        ],
    )
}

/// Unix time of the last completed sync, if the wallet has ever synced.
pub fn last_completed_sync_at(db_path: &str) -> Result<Option<u64>, String> {
    let conn = open_readonly_conn_with_timeout(db_path, Some(SYNC_DB_BUSY_TIMEOUT))?;
    read_sync_meta(&conn, LAST_COMPLETED_SYNC_AT_KEY)
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
) -> Result<Option<u64>, SyncError> {
    let checked_version = open_readonly_conn_with_timeout(db_data_path, Some(SYNC_DB_BUSY_TIMEOUT))
        .and_then(|conn| read_sync_meta::<String>(&conn, WITNESS_CHECK_VERSION_KEY))
        .unwrap_or_else(|e| {
            log::warn!("sync: witness repair metadata unavailable, running check: {e}");
            None
        });
    if checked_version.as_deref() == Some(env!("CARGO_PKG_VERSION")) {
        return Ok(None);
    }
    log::info!("sync: witness repair check running");

    let rescan_ranges = with_wallet_db_write_lock("sync_engine.check_witnesses", || {
        db.check_witnesses()
            .map_err(|e| SyncError::db(format!("check_witnesses: {e}")))
    })?;

    let Some(nonempty_ranges) = NonEmpty::from_vec(rescan_ranges) else {
        if let Err(e) = with_wallet_db_write_lock("sync_engine.mark_witness_check_clean", || {
            mark_witness_check_clean(db_data_path)
        }) {
            log::warn!("sync: witness repair clean marker update failed: {e}");
        } else {
            log::info!("sync: witness repair check found no work; marked clean for this version");
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
        "sync: witness repair pass {}/{} queued {} range(s), {} block(s) \
         (first={first})",
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
            "sync: anchor root mismatch at {anchor_height} \
             (target={}, repair_height={repair_height}, pass {}/{}); \
             local_sapling={:?}, canonical_sapling={:?}, local_orchard={:?}, \
             canonical_orchard={:?}, local_ironwood={:?}, canonical_ironwood={:?}; \
             rewinding to canonical chain state",
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
                    "sync: anchor root repair at {repair_height} conflicted \
                     with an existing tree root; trying a deeper repair if available ({conflict})",
                );
                last_root_conflict = Some(conflict);
                continue;
            }
        };

        let pending_blocks = pending_scan_blocks(&post_rewind_ranges);
        let first_pending =
            first_pending_scan_range(&post_rewind_ranges).unwrap_or_else(|| "none".into());
        log::info!(
            "sync: anchor root repair queued {pending_blocks} block(s) \
             (first_pending={first_pending})",
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

// ==================== Main sync ====================

/// Run the full sync loop with automatic retry on failure.
/// Retries up to 3 times with exponential backoff (2s, 4s, 8s).
pub async fn run_sync_inner(
    db_data_path: &str,
    client: &LwdClient,
    network: WalletNetwork,
    progress_fn: impl Fn(SyncProgressEvent) + Send + Sync,
) -> Result<(), String> {
    const MAX_RETRIES: u32 = 3;
    crate::account::ensure_db_migrated_once(db_data_path, network)?;
    let mut last_err = String::new();

    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            let delay_secs = 1u64 << attempt; // 2, 4, 8
            log::warn!(
                "sync: retry {}/{} in {}s (error: {})",
                attempt,
                MAX_RETRIES,
                delay_secs,
                last_err
            );
            tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
        }

        match run_sync_impl(db_data_path, client, network, &progress_fn).await {
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
                // phase budget. Treat it as a retry-worthy
                // transient: the next attempt gets a fresh rewind budget,
                // which is often enough to get past a multi-level reorg
                // that couldn't be cleared in one run.
                let strategy = sync_err.recovery_strategy();
                let err_string = sync_err.to_string();
                match strategy {
                    RecoveryStrategy::Fatal => {
                        log::error!("sync: fatal error, not retrying: {err_string}");
                        return Err(err_string);
                    }
                    RecoveryStrategy::RetryWithBackoff | RecoveryStrategy::Rewind { .. } => {
                        last_err = err_string;
                        if attempt == MAX_RETRIES {
                            log::error!("sync: all {} retries exhausted", MAX_RETRIES);
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
    client: &LwdClient,
    network: WalletNetwork,
    progress_fn: &(impl Fn(SyncProgressEvent) + Send + Sync),
) -> Result<(), SyncError> {
    let default_batch_size = BATCH_SIZE;
    let base_batch_size = effective_base_batch_size(default_batch_size);
    log::info!("sync: starting (base_batch={})", base_batch_size);

    // Persist the active session before any new sync work begins. A crash, or
    // the process being killed or replaced mid-sync, cannot leave the previous
    // completed tip looking like the current run completed successfully.
    with_wallet_db_write_lock("sync_engine.mark_sync_started", || {
        mark_sync_started(db_data_path)
    })
    .map_err(SyncError::db)?;

    // 1. Share the caller's gRPC connection.
    let mut client = client.clone();

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
    log::info!("sync: chain tip = {}", tip.height);

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

    refresh_utxos(&mut client, &mut db).await?;

    // 2.5. Resubmit eligible unmined wallet txs now that we know the
    // current tip. Best-effort: failures are logged inside the helper
    // and must not abort the sync.
    //
    // We reuse the same `client` instead of opening a fresh channel.
    let _ =
        crate::sync::resubmit_pending_transactions(db_data_path, &mut client, tip.height as u32)
            .await;

    // 3. Download subtree roots (incremental)
    download_subtree_roots(&mut client, &mut db).await?;

    // 4. Calculate initial scan target (before any scanning)
    let initial_ranges = db
        .suggest_scan_ranges()
        .map_err(|e| SyncError::db(format!("suggest_scan_ranges: {e}")))?;
    log::info!(
        "sync: {} blocks to scan",
        pending_scan_blocks(&initial_ranges)
    );

    // Bounded counters for reorg-triggered rewinds inside this one sync run,
    // split between the verify phase and the main scan phase. Separate
    // budgets keep a flapping verify range from eating the main scan's
    // rewind budget.
    let mut verify_rewinds_this_run: u32 = 0;
    let mut main_rewinds_this_run: u32 = 0;
    let mut witness_repair_passes_this_run: u32 = 0;
    let mut anchor_root_repair_passes_this_run: u32 = 0;

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
    // work (scan) and unrelated gRPC calls (resubmit).
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
    /// Prefetched block download state. The `JoinSet` aborts the spawned
    /// task when dropped, so detached downloads can't outlive the sync
    /// session and leak network traffic after shutdown.
    struct Prefetch {
        task: tokio::task::JoinSet<PrefetchResult>,
        start: BlockHeight,
        end: BlockHeight,
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
            log::info!("sync: entering verify phase");
            verify_phase_announced = true;
        } else if !is_verify_phase && !main_phase_announced {
            if verify_phase_announced {
                log::info!("sync: verify phase complete, entering main scan");
            } else {
                log::info!("sync: entering main scan phase (no verify work)");
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
        // using more memory.
        let Some((batch_size, end)) =
            scannable_batch_end(base_batch_size, start, range_end, current_tip_height)
        else {
            log::info!(
                "sync: pending range {} starts after current tip {}, waiting for tip advance",
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
            "sync: scanning {}-{} (priority {:?}{}, batch={})",
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
                match pf.task.join_next().await {
                    Some(Ok(Ok((bs, ts)))) => {
                        log::debug!(
                            "sync: using prefetched blocks for {}-{}",
                            u32::from(start),
                            u32::from(end) - 1,
                        );
                        (bs, Some(ts))
                    }
                    _ => {
                        // Prefetch failed — download synchronously.
                        log::warn!("sync: prefetch failed, downloading fresh");
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
                let mut task = tokio::task::JoinSet::new();
                task.spawn(async move {
                    let blocks = download_blocks(&mut pf_client, pf_start, pf_end - 1).await?;
                    let tree_state =
                        get_tree_state(&mut pf_client, u32::from(pf_start - 1) as u64).await?;
                    Ok((blocks, tree_state))
                });
                prefetch = Some(Prefetch {
                    start: pf_start,
                    end: pf_end,
                    task,
                });
            } else {
                log::debug!(
                    "sync: skipping prefetch from {} past current tip {}",
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
                            "sync: {phase_name} rewind budget exhausted \
                             ({}/{}); propagating error",
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
                                        "sync: {phase_name} rewind target {requested_height} \
                                         below earliest checkpoint; retrying at safe_rewind_height={safe}",
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
                                        "sync: {phase_name} rewind to {requested_height} \
                                         rejected and no safe_rewind_height is available; \
                                         cannot recover from this reorg in-place",
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
                    let summary = sync::wallet_scan_heights(&db).map_err(SyncError::db)?;
                    let actual_rewind_height_u64 = u32::from(actual_rewind_height) as u64;
                    log::info!(
                        "sync: {phase_name} rewound to {actual_rewind_height} \
                         after reorg (requested={requested_rewind_height}, \
                         distance={rewind_distance}, attempt {}/{}); \
                         post_rewind_pending={post_rewind_pending}, first_pending={first_pending}, \
                         summary={summary:?}; restarting scan loop",
                        *current_rewinds,
                        MAX_REWINDS_PER_RUN,
                    );
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

        let post_scan_ranges = db
            .suggest_scan_ranges()
            .map_err(|e| SyncError::db(format!("suggest_scan_ranges: {e}")))?;

        // Enhancement
        run_enhancement(&mut client, &mut db).await?;

        // Post-batch auto-resubmit.
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
        // closes the "resubmit expired tx forever" regression.
        // The request
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
                // never actually scanned.
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
                                "sync: post-batch update_chain_tip({fresh_tip_height}) \
                                 failed, keeping tip at {current_tip_height}: {e}",
                            );
                        }
                    }
                }
                let _ = crate::sync::resubmit_pending_transactions(
                    db_data_path,
                    &mut client,
                    fresh_tip_height,
                )
                .await;
            }
            Err(e) => {
                log::warn!("sync: resubmit tip refresh failed, skipping pass: {e}");
            }
        }
        // Report progress
        let remaining = pending_scan_blocks(&post_scan_ranges);
        let progress = SyncProgressEvent {
            scanned_height: u32::from(end) as u64,
            chain_tip_height: current_tip_height,
        };
        log::info!("sync: {} blocks remaining", remaining);
        progress_fn(progress);
        #[cfg(debug_assertions)]
        maybe_sleep_for_e2e_sync_batch_delay().await;
    }

    let (final_scanned_height, final_tip_height) =
        ensure_complete_scan_state(&mut db, current_tip_height)?;
    log::info!(
        "sync: completed (fully_scanned={}, chain_tip={})",
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

/// The birthday of an account whose first block is `height`: empty note
/// commitment trees at or below Sapling activation, otherwise the tree state
/// the server reports for block `height - 1`.
pub async fn fetch_birthday(
    client: &mut LwdClient,
    network: WalletNetwork,
    height: u64,
) -> Result<AccountBirthday, String> {
    let start = BlockHeight::try_from(height)
        .map_err(|_| format!("Birthday {height} is not a valid block height"))?;
    if should_use_empty_chain_state(&network, start).map_err(|e| e.to_string())? {
        let prior = chain::ChainState::empty(start - 1, BlockHash([0u8; 32]));
        return Ok(AccountBirthday::from_parts(prior, None));
    }
    let tree_state = get_tree_state(client, height - 1)
        .await
        .map_err(|e| e.to_string())?;
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
