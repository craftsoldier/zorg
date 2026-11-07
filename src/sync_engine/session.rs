use rusqlite::{params, OptionalExtension};

use crate::{
    db::{
        open_readonly_conn_with_timeout, open_wallet_raw_conn_with_timeout, SYNC_DB_BUSY_TIMEOUT,
    },
    network::WalletNetwork,
};

use super::{run_sync_impl, LwdClient, RecoveryStrategy, SyncProgressEvent};

const SYNC_META_TABLE: &str = "ext_zorg_sync_meta";
const LAST_COMPLETED_SYNC_HEIGHT_KEY: &str = "last_completed_sync_height";
const SYNC_IN_PROGRESS_KEY: &str = "sync_in_progress";
const LAST_COMPLETED_SYNC_AT_KEY: &str = "last_completed_sync_at";
pub(super) const WITNESS_CHECK_VERSION_KEY: &str = "witness_check_version";
type SyncCompletionMeta = (Option<u64>, Option<bool>);

pub(super) fn read_sync_meta<T>(conn: &rusqlite::Connection, key: &str) -> Result<Option<T>, String>
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

pub(super) fn mark_witness_check_clean(db_data_path: &str) -> Result<(), String> {
    write_sync_meta(
        db_data_path,
        &[(WITNESS_CHECK_VERSION_KEY, env!("CARGO_PKG_VERSION").into())],
    )
}

pub(crate) fn completed_sync_height_for_status(db_data_path: &str) -> Result<Option<u64>, String> {
    let (completed_height, in_progress) = read_sync_completion_meta(db_data_path)?;
    Ok(completed_height.filter(|_| in_progress != Some(true)))
}

pub(super) fn mark_sync_started(db_data_path: &str) -> Result<(), String> {
    write_sync_meta(db_data_path, &[(SYNC_IN_PROGRESS_KEY, "1".into())])
}

pub(super) fn mark_sync_completed(
    db_data_path: &str,
    completed_tip_height: u64,
) -> Result<(), String> {
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
