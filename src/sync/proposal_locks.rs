//! Durable bookkeeping for owner-scoped locks held by in-memory send proposals.
//!
//! Send proposals live only in the process, but the wallet locks they
//! own live in SQLite. This side table records enough generic `OutputRef`
//! information to release locks left by a previous process without touching
//! long-lived migration owners.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use rand::{rand_core::UnwrapErr, rngs::SysRng, Rng};
use rusqlite::{params, Connection};
use zcash_client_backend::{
    data_api::OutputLockStore,
    wallet::{LockOwner, OutputRef},
};
use zcash_primitives::transaction::TxId;
use zcash_protocol::{PoolType, ShieldedPool};

use crate::{
    db::{open_wallet_raw_conn_with_timeout, with_wallet_db_write_lock, READ_DB_BUSY_TIMEOUT},
    network::WalletNetwork,
};

use super::open_wallet_db;

const TABLE: &str = "zorg_send_proposal_locks";

type PreviousSessionLocks = BTreeMap<LockOwner, Vec<OutputRef>>;

static PROCESS_SESSION_ID: LazyLock<[u8; 16]> = LazyLock::new(|| {
    let mut bytes = [0; 16];
    UnwrapErr(SysRng).fill_bytes(&mut bytes);
    bytes
});

fn pool_code(pool: PoolType) -> i64 {
    match pool {
        PoolType::Transparent => 0,
        PoolType::Shielded(ShieldedPool::Sapling) => 2,
        PoolType::Shielded(ShieldedPool::Orchard) => 3,
        PoolType::Shielded(ShieldedPool::Ironwood) => 4,
    }
}

fn pool_from_code(code: i64) -> Result<PoolType, String> {
    match code {
        0 => Ok(PoolType::TRANSPARENT),
        2 => Ok(PoolType::SAPLING),
        3 => Ok(PoolType::ORCHARD),
        4 => Ok(PoolType::IRONWOOD),
        _ => Err(format!("Unknown persisted send lock pool code {code}")),
    }
}

fn ensure_schema(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS {TABLE} (
            owner BLOB NOT NULL,
            session_id BLOB NOT NULL,
            txid BLOB NOT NULL,
            pool INTEGER NOT NULL,
            output_index INTEGER NOT NULL,
            PRIMARY KEY (owner, txid, pool, output_index)
        );
        CREATE INDEX IF NOT EXISTS idx_zorg_send_proposal_locks_session
            ON {TABLE}(session_id);"
    ))
    .map_err(|e| format!("Initialize send proposal lock recovery schema: {e}"))
}

pub(super) fn persist(
    db_path: &str,
    owner: LockOwner,
    outputs: &[OutputRef],
) -> Result<(), String> {
    let mut conn = open_wallet_raw_conn_with_timeout(db_path, READ_DB_BUSY_TIMEOUT)?;
    ensure_schema(&conn)?;
    let tx = conn
        .transaction()
        .map_err(|e| format!("Begin send proposal lock persistence: {e}"))?;
    for output in outputs {
        tx.execute(
            &format!(
                "INSERT OR REPLACE INTO {TABLE}
                    (owner, session_id, txid, pool, output_index)
                 VALUES (?1, ?2, ?3, ?4, ?5)"
            ),
            params![
                owner.as_bytes().as_slice(),
                PROCESS_SESSION_ID.as_slice(),
                output.txid().as_ref(),
                pool_code(output.pool()),
                output.output_index(),
            ],
        )
        .map_err(|e| format!("Persist send proposal input lock: {e}"))?;
    }
    tx.commit()
        .map_err(|e| format!("Commit send proposal lock persistence: {e}"))
}

pub(super) fn remove(db_path: &str, owner: LockOwner) -> Result<(), String> {
    let conn = open_wallet_raw_conn_with_timeout(db_path, READ_DB_BUSY_TIMEOUT)?;
    ensure_schema(&conn)?;
    conn.execute(
        &format!("DELETE FROM {TABLE} WHERE owner = ?1"),
        params![owner.as_bytes().as_slice()],
    )
    .map_err(|e| format!("Remove send proposal lock recovery rows: {e}"))?;
    Ok(())
}

fn read_previous_session_locks(conn: &Connection) -> Result<PreviousSessionLocks, String> {
    ensure_schema(conn)?;
    let mut stmt = conn
        .prepare(&format!(
            "SELECT owner, txid, pool, output_index
             FROM {TABLE}
             WHERE session_id <> ?1
             ORDER BY owner, txid, pool, output_index"
        ))
        .map_err(|e| format!("Prepare orphan send lock recovery query: {e}"))?;
    let rows = stmt
        .query_map(params![PROCESS_SESSION_ID.as_slice()], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, u32>(3)?,
            ))
        })
        .map_err(|e| format!("Query orphan send locks: {e}"))?;

    let mut locks = PreviousSessionLocks::new();
    for row in rows {
        let (owner_bytes, txid_bytes, pool, output_index) =
            row.map_err(|e| format!("Read orphan send lock: {e}"))?;
        let owner = LockOwner::new(
            owner_bytes
                .try_into()
                .map_err(|_| "Persisted send lock owner is not 32 bytes")?,
        );
        let txid = TxId::from_bytes(
            txid_bytes
                .try_into()
                .map_err(|_| "Persisted send lock txid is not 32 bytes")?,
        );
        locks.entry(owner).or_default().push(OutputRef::new(
            txid,
            pool_from_code(pool)?,
            output_index,
        ));
    }
    Ok(locks)
}

/// Releases the send locks left by an earlier process. Migration locks are
/// never registered in this table and therefore cannot be cleared by this
/// recovery pass.
pub(crate) fn recover_previous_process(
    db_path: &str,
    network: WalletNetwork,
) -> Result<(), String> {
    with_wallet_db_write_lock("proposal_locks.recover_previous_process", || {
        let conn = open_wallet_raw_conn_with_timeout(db_path, READ_DB_BUSY_TIMEOUT)?;
        let locks = read_previous_session_locks(&conn)?;
        if locks.is_empty() {
            return Ok(());
        }
        drop(conn);

        let mut db = open_wallet_db(db_path, network)?;
        for (owner, outputs) in &locks {
            for output in outputs {
                db.unlock_output(output, *owner)
                    .map_err(|e| format!("Release orphan send proposal input: {e}"))?;
            }
        }
        drop(db);

        let mut conn = open_wallet_raw_conn_with_timeout(db_path, READ_DB_BUSY_TIMEOUT)?;
        ensure_schema(&conn)?;
        let tx = conn
            .transaction()
            .map_err(|e| format!("Begin orphan send lock cleanup: {e}"))?;
        for owner in locks.keys() {
            tx.execute(
                &format!("DELETE FROM {TABLE} WHERE owner = ?1"),
                params![owner.as_bytes().as_slice()],
            )
            .map_err(|e| format!("Delete recovered send lock rows: {e}"))?;
        }
        tx.commit()
            .map_err(|e| format!("Commit orphan send lock cleanup: {e}"))?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_pool_codes_round_trip() {
        for pool in [
            PoolType::TRANSPARENT,
            PoolType::SAPLING,
            PoolType::ORCHARD,
            PoolType::IRONWOOD,
        ] {
            assert_eq!(pool_from_code(pool_code(pool)).unwrap(), pool);
        }
    }

    #[test]
    fn current_session_rows_are_not_orphans() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        conn.execute(
            &format!(
                "INSERT INTO {TABLE}
                    (owner, session_id, txid, pool, output_index)
                 VALUES (?1, ?2, ?3, 3, 0)"
            ),
            params![
                [1u8; 32].as_slice(),
                PROCESS_SESSION_ID.as_slice(),
                [2u8; 32].as_slice(),
            ],
        )
        .unwrap();
        assert!(read_previous_session_locks(&conn).unwrap().is_empty());
    }
}
