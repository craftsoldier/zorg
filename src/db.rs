use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use rand::{rand_core::UnwrapErr, rngs::SysRng};
use zcash_client_backend::data_api::{wallet::ConfirmationsPolicy, WalletRead, WalletSummary};
use zcash_client_sqlite::{util::SystemClock, AccountUuid, WalletDb};

use crate::network::WalletNetwork;

pub(crate) type WalletDatabase =
    WalletDb<rusqlite::Connection, WalletNetwork, SystemClock, UnwrapErr<SysRng>>;

/// User-driven wallet operations can afford a longer wait for a short sync write.
pub(crate) const WALLET_DB_BUSY_TIMEOUT: Duration = Duration::from_secs(10);
/// Account creation/import runs after sync is paused, so a shorter wait exposes real stalls.
pub(crate) const ACCOUNT_MUTATION_DB_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// The sync loop should absorb brief read/write overlap without stretching cancel too far.
pub(crate) const SYNC_DB_BUSY_TIMEOUT: Duration = Duration::from_secs(2);
pub(crate) const READ_DB_BUSY_TIMEOUT: Duration = Duration::from_secs(2);

/// `get_wallet_summary` for the default confirmation policy, read on a new connection.
pub(crate) fn wallet_summary(
    db_path: &str,
    network: WalletNetwork,
) -> Result<Option<WalletSummary<AccountUuid>>, String> {
    open_wallet_db_for_read_with_timeout(db_path, network, READ_DB_BUSY_TIMEOUT)?
        .get_wallet_summary(ConfirmationsPolicy::default())
        .map_err(|e| e.to_string())
}

pub(crate) fn open_wallet_db_with_timeout(
    db_path: &str,
    network: WalletNetwork,
    timeout: Duration,
) -> Result<WalletDatabase, String> {
    let conn = rusqlite::Connection::open(db_path)
        .map_err(|e| format!("Failed to open wallet DB: {e}"))?;
    configure_wallet_connection(&conn, timeout, true)?;
    Ok(WalletDb::from_connection(
        conn,
        network,
        SystemClock,
        UnwrapErr(SysRng),
    ))
}

pub(crate) fn open_wallet_db_for_read_with_timeout(
    db_path: &str,
    network: WalletNetwork,
    timeout: Duration,
) -> Result<WalletDatabase, String> {
    let conn = rusqlite::Connection::open(db_path)
        .map_err(|e| format!("Failed to open wallet DB: {e}"))?;
    configure_wallet_connection(&conn, timeout, false)?;
    Ok(WalletDb::from_connection(
        conn,
        network,
        SystemClock,
        UnwrapErr(SysRng),
    ))
}

pub(crate) fn open_wallet_raw_conn_with_timeout(
    db_path: &str,
    timeout: Duration,
) -> Result<rusqlite::Connection, String> {
    let conn = rusqlite::Connection::open(db_path)
        .map_err(|e| format!("Failed to open wallet DB: {e}"))?;
    configure_wallet_connection(&conn, timeout, true)?;
    Ok(conn)
}

fn configure_wallet_connection(
    conn: &rusqlite::Connection,
    timeout: Duration,
    ensure_wal: bool,
) -> Result<(), String> {
    conn.busy_timeout(timeout)
        .map_err(|e| format!("Failed to configure wallet DB busy timeout: {e}"))?;
    if ensure_wal {
        let journal_mode: String = conn
            .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
            .map_err(|e| format!("Failed to enable wallet DB WAL mode: {e}"))?;
        if !journal_mode.eq_ignore_ascii_case("wal") {
            return Err(format!(
                "Failed to enable wallet DB WAL mode: SQLite returned journal_mode={journal_mode}"
            ));
        }
    }
    rusqlite::vtab::array::load_module(conn)
        .map_err(|e| format!("Failed to load SQLite array module: {e}"))?;
    Ok(())
}

pub(crate) fn with_wallet_db_write_lock<T>(
    operation: &'static str,
    write: impl FnOnce() -> T,
) -> T {
    // Serializes wallet-DB writes across foreground
    // calls and Rust sync tasks inside this process. This
    // does not coordinate with a separate OS process that opens the same DB.
    static WALLET_DB_WRITE_LOCK: Mutex<()> = Mutex::new(());

    let wait_start = Instant::now();
    let guard = match WALLET_DB_WRITE_LOCK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            log::error!("wallet DB write lock poisoned while entering {operation}; continuing");
            poisoned.into_inner()
        }
    };

    let waited = wait_start.elapsed();
    if waited >= Duration::from_millis(50) {
        log::info!(
            "wallet DB write lock waited {:.3}s for {operation}",
            waited.as_secs_f64()
        );
    }

    let hold_start = Instant::now();
    let result = write();
    let held = hold_start.elapsed();
    if held >= Duration::from_secs(1) {
        log::info!(
            "wallet DB write lock held {:.3}s by {operation}",
            held.as_secs_f64()
        );
    }

    drop(guard);
    result
}

pub(crate) fn open_readonly_conn_with_timeout(
    db_path: &str,
    timeout: Option<Duration>,
) -> Result<rusqlite::Connection, String> {
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("Failed to open DB: {e}"))?;
    if let Some(timeout) = timeout {
        conn.busy_timeout(timeout)
            .map_err(|e| format!("Failed to configure DB busy timeout: {e}"))?;
    }
    // Same module write paths get via `configure_wallet_connection`.
    // Needed so read-only history queries can bind txid sets with `rarray`
    // instead of re-scanning `v_transactions` (and its `raw` blobs).
    rusqlite::vtab::array::load_module(&conn)
        .map_err(|e| format!("Failed to load SQLite array module: {e}"))?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    #[test]
    fn configure_wallet_connection_enables_wal_mode() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let conn = rusqlite::Connection::open(file.path()).unwrap();

        configure_wallet_connection(&conn, Duration::from_millis(1), true).unwrap();

        let journal_mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_eq!(journal_mode.to_ascii_lowercase(), "wal");
    }

    #[test]
    fn configure_wallet_connection_can_skip_wal_for_read_paths() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let conn = rusqlite::Connection::open(file.path()).unwrap();

        configure_wallet_connection(&conn, Duration::from_millis(1), false).unwrap();

        let journal_mode: String = conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .unwrap();
        assert_ne!(journal_mode.to_ascii_lowercase(), "wal");
    }

    #[test]
    fn with_wallet_db_write_lock_runs_closure() {
        let mut called = false;

        with_wallet_db_write_lock("test", || {
            called = true;
        });

        assert!(called);
    }

    #[test]
    fn write_lock_is_released_on_unwind() {
        let result = catch_unwind(AssertUnwindSafe(|| {
            with_wallet_db_write_lock("test_panic", || {
                panic!("force unwind inside the write lock");
            });
        }));
        assert!(result.is_err());

        // A poisoned lock must still be acquirable; otherwise this would hang.
        with_wallet_db_write_lock("test_after_panic", || {});
    }
}
