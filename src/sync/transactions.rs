//! Read-only transaction / balance / pending-tx query surface.
//!
//! Everything in this module is an "ask the wallet a question"
//! helper that the caller
//! calls per user action:
//!
//!   - Balance / address queries (`get_wallet_balance`,
//!     `get_next_available_address`).
//!   - Transaction list + on-chain enhancement requests
//!     (`get_transaction_history`, `get_transaction_data_requests`,
//!     `decrypt_and_store_transaction`, `set_transaction_status`).
//!
//! None of these belong to the orchestration loop — the loop lives
//! in `sync_engine.rs`. They're one-shot lookups, so extracting them into their own submodule keeps
//! `sync.rs` focused on per-wallet infrastructure (DB open,
//! chain-tip update, scan range management).

use std::{
    collections::{HashMap, HashSet},
    rc::Rc,
};

use rusqlite::{types::Value, vtab::array::Array, OptionalExtension};
use transparent::address::TransparentAddress;
use zcash_client_backend::data_api::{wallet::ConfirmationsPolicy, Balance, WalletWrite};
use zcash_primitives::transaction::{Transaction, TxId};
use zcash_protocol::{consensus::BranchId, memo::Memo, value::Zatoshis};

use crate::account::{parse_account_uuid, Account};
use crate::db::with_wallet_db_write_lock;
use crate::network::WalletNetwork;

use super::{open_readonly_conn, open_wallet_db, open_wallet_db_for_read};

const TRANSPARENT_POOL: i64 = 0;
const SAPLING_POOL: i64 = 2;
const ORCHARD_POOL: i64 = 3;
const IRONWOOD_POOL: i64 = 4;

// ======================== Balance ========================

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WalletBalance {
    pub transparent: u64,
    pub sapling: u64,
    pub orchard: u64,
    pub ironwood: u64,
    pub transparent_locked: u64,
    pub transparent_pending: u64,
    pub change_pending_confirmation: u64,
    pub value_pending_spendability: u64,
    /// Sum of spendable shielded balances. Use this for "available to send".
    pub spendable: u64,
    /// Sum of owner-locked balances across all pools.
    pub locked: u64,
    /// Sum of spendable + locked + pending balances across all pools.
    pub total: u64,
}

pub fn get_wallet_balance(account: &Account) -> Result<WalletBalance, String> {
    let account_id = parse_account_uuid(&account.uuid)?;
    let snapshot = open_wallet_db_for_read(&account.db_path, account.network)?
        .get_wallet_snapshot(ConfirmationsPolicy::default())
        .map_err(|e| e.to_string())?;
    let Some(b) = snapshot
        .as_ref()
        .and_then(|s| s.account_balances().get(&account_id))
    else {
        return Ok(WalletBalance::default());
    };
    let pools = [
        b.unshielded_balance(),
        *b.sapling_balance(),
        *b.orchard_balance(),
        *b.ironwood_balance(),
    ];
    let sum =
        |value: fn(&Balance) -> Zatoshis| pools.iter().map(|p| u64::from(value(p))).sum::<u64>();
    let [transparent, sapling, orchard, ironwood] = pools.map(|p| u64::from(p.spendable_value()));
    let (change, pending, locked) = (
        sum(Balance::change_pending_confirmation),
        sum(Balance::value_pending_spendability),
        sum(Balance::locked_value),
    );
    let unshielded = pools[0];
    Ok(WalletBalance {
        transparent,
        sapling,
        orchard,
        ironwood,
        transparent_locked: u64::from(unshielded.locked_value()),
        transparent_pending: u64::from(unshielded.change_pending_confirmation())
            + u64::from(unshielded.value_pending_spendability()),
        change_pending_confirmation: change,
        value_pending_spendability: pending,
        spendable: sapling + orchard + ironwood,
        locked,
        total: transparent + sapling + orchard + ironwood + locked + change + pending,
    })
}

// ======================== Diversified Address ========================

pub fn get_next_available_address(account: &Account) -> Result<String, String> {
    let account_id = parse_account_uuid(&account.uuid)?;

    let (ua, _) = with_wallet_db_write_lock("transactions.get_next_available_address", || {
        let mut db = open_wallet_db(&account.db_path, account.network)?;
        db.get_next_available_address(account_id, crate::keys::shielded_address_request())
            .map_err(|e| match e {
                zcash_client_sqlite::error::SqliteClientError::ChainHeightUnknown => {
                    "Sync this account before making a new address".to_string()
                }
                e => format!("{e}"),
            })?
            .ok_or_else(|| "No address available".to_string())
    })?;
    Ok(ua.encode(&account.network))
}

// ======================== Transaction History ========================

pub struct TransactionInfo {
    pub txid: TxId,
    pub mined_height: u64,
    pub expired_unmined: bool,
    pub fee: u64,
    pub block_time: u64,
    pub tx_kind: String,
    pub display_amount: u64,
    pub display_pool: String,
}

pub struct TransactionDetail {
    pub source_address: Option<String>,
    pub outputs: Vec<TransactionDetailOutput>,
}

pub struct TransactionDetailOutput {
    pub address: Option<String>,
    pub amount_zatoshi: u64,
    pub memo: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TxBase {
    txid: Vec<u8>,
    mined_height: Option<u32>,
    expired_unmined: bool,
    expiry_height: Option<u32>,
    account_balance_delta: i64,
    fee: u64,
    block_time: u64,
    total_spent: u64,
    total_received: u64,
    is_shielding: bool,
    tx_index: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TxOutput {
    txid: Vec<u8>,
    output_pool: i64,
    output_index: i64,
    from_account_uuid: Option<Vec<u8>>,
    to_account_uuid: Option<Vec<u8>>,
    to_address: Option<String>,
    to_key_scope: Option<i64>,
    value: u64,
    memo: Option<Vec<u8>>,
}

#[derive(Default, Clone)]
struct ActivityAmounts {
    amount: u64,
    has_transparent: bool,
    has_shielded: bool,
    has_ironwood: bool,
}

impl ActivityAmounts {
    fn add_output(&mut self, output: &TxOutput) {
        self.amount = self.amount.saturating_add(output.value);
        match output.output_pool {
            TRANSPARENT_POOL => self.has_transparent = true,
            SAPLING_POOL | ORCHARD_POOL => self.has_shielded = true,
            IRONWOOD_POOL => self.has_ironwood = true,
            _ => {}
        }
    }

    fn display_pool(&self) -> &'static str {
        match (self.has_transparent, self.has_shielded, self.has_ironwood) {
            (true, false, false) => "transparent",
            (false, true, false) => "shielded",
            (false, false, true) => "ironwood",
            (false, false, false) => "unknown",
            _ => "mixed",
        }
    }
}

#[derive(Default, Clone)]
struct ActivitySummary {
    sent: ActivityAmounts,
    received: ActivityAmounts,
    shielded: ActivityAmounts,
}

struct ClassifiedTx {
    info: TransactionInfo,
    sort_pending_rank: u8,
    sort_height: u64,
    tx_index: i64,
    row_order: u8,
}

pub fn get_transaction_history(
    account: &Account,
    limit: Option<u32>,
) -> Result<Vec<TransactionInfo>, String> {
    let uuid = uuid::Uuid::parse_str(&account.uuid).map_err(|e| format!("Invalid UUID: {e}"))?;
    let uuid_bytes = uuid.as_bytes().to_vec();

    // Open a separate read-only connection (WalletDb.conn is private).
    let conn = open_readonly_conn(&account.db_path)?;
    let read_tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("SQL error: {e}"))?;
    let bases = read_history_bases(&read_tx, &uuid_bytes, None)?;
    if bases.is_empty() {
        return Ok(Vec::new());
    }

    // Bind the txids already in `bases` instead of re-querying
    // `v_transactions` for DISTINCT txid. That view selects
    // `transactions.raw`, so a second pass would re-materialize every
    // raw blob even though this path never reads them.
    let outputs_by_txid = read_history_outputs(
        &read_tx,
        &uuid_bytes,
        bases.iter().map(|base| base.txid.as_slice()),
    )?;
    drop(read_tx);

    Ok(assemble_history(
        &bases,
        &outputs_by_txid,
        &uuid_bytes,
        limit,
    ))
}

/// Turn raw history rows into the display list: summarize, classify, filter,
/// sort, truncate. It does no database access.
fn assemble_history(
    bases: &[TxBase],
    outputs_by_txid: &HashMap<Vec<u8>, Vec<TxOutput>>,
    uuid_bytes: &[u8],
    limit: Option<u32>,
) -> Vec<TransactionInfo> {
    let mut visible: Vec<ClassifiedTx> = bases
        .iter()
        .flat_map(|base| {
            let outputs = outputs_by_txid
                .get(&base.txid)
                .map_or(&[][..], Vec::as_slice);
            classify_history_tx(base, &summarize_activity_outputs(base, outputs, uuid_bytes))
        })
        .collect();

    visible.retain(|tx| {
        tx.info.display_amount > 0 || tx.info.tx_kind == "unknown" || tx.info.tx_kind == "shielded"
    });

    visible.sort_by(|a, b| {
        b.sort_pending_rank
            .cmp(&a.sort_pending_rank)
            .then_with(|| b.sort_height.cmp(&a.sort_height))
            .then_with(|| b.tx_index.cmp(&a.tx_index))
            .then_with(|| b.info.txid.cmp(&a.info.txid))
            .then_with(|| a.row_order.cmp(&b.row_order))
    });

    if let Some(limit) = limit {
        visible.truncate(limit as usize);
    }

    visible.into_iter().map(|tx| tx.info).collect()
}

pub fn get_transaction_detail(
    account: &Account,
    txid: &TxId,
    tx_kind: &str,
) -> Result<TransactionDetail, String> {
    let uuid = uuid::Uuid::parse_str(&account.uuid).map_err(|e| format!("Invalid UUID: {e}"))?;
    let uuid_bytes = uuid.as_bytes().to_vec();

    let conn = open_readonly_conn(&account.db_path)?;
    let read_tx = conn
        .unchecked_transaction()
        .map_err(|e| format!("SQL error: {e}"))?;
    let Some(base) = read_history_bases(&read_tx, &uuid_bytes, Some(txid.as_ref()))?.pop() else {
        return Err("Transaction not found".to_string());
    };
    let txid_bytes: &[u8] = txid.as_ref();
    let mut outputs = read_history_outputs(&read_tx, &uuid_bytes, [txid_bytes])?
        .remove(txid_bytes)
        .unwrap_or_default();
    outputs.sort_by(|a, b| {
        a.output_index
            .cmp(&b.output_index)
            .then_with(|| a.output_pool.cmp(&b.output_pool))
    });

    let visible_outputs = outputs
        .iter()
        .filter(|output| detail_includes_output(&base, output, uuid_bytes.as_slice(), tx_kind))
        .collect::<Vec<_>>();
    let source_address =
        if matches!(tx_kind, "received" | "receiving") && !visible_outputs.is_empty() {
            let raw_tx = read_raw_transaction_for_tx(&read_tx, &uuid_bytes, txid.as_ref())?;
            received_source_from_raw_transaction(account.network, raw_tx.as_deref())
        } else {
            None
        };
    let outputs = visible_outputs
        .into_iter()
        .map(|output| TransactionDetailOutput {
            address: output.to_address.clone(),
            amount_zatoshi: output.value,
            memo: decode_text_memo(output.memo.as_deref()),
        })
        .collect();

    Ok(TransactionDetail {
        source_address,
        outputs,
    })
}

fn received_source_from_raw_transaction(
    network: WalletNetwork,
    raw_tx: Option<&[u8]>,
) -> Option<String> {
    let tx = Transaction::read(raw_tx?, BranchId::Sapling).ok()?;
    tx.transparent_bundle()?.vin.iter().find_map(|input| {
        transparent_source_address_from_script_sig(network, input.script_sig().0 .0.as_slice())
    })
}

fn transparent_source_address_from_script_sig(
    network: WalletNetwork,
    script_sig: &[u8],
) -> Option<String> {
    let pubkey = parse_standard_p2pkh_pubkey_from_script_sig(script_sig)?;
    let address = TransparentAddress::PublicKeyHash(transparent::util::hash160::hash(pubkey));
    Some(zcash_keys::encoding::encode_transparent_address_p(
        &network, &address,
    ))
}

fn parse_standard_p2pkh_pubkey_from_script_sig(script_sig: &[u8]) -> Option<&[u8]> {
    let pushes = parse_script_pushes(script_sig)?;
    pushes
        .into_iter()
        .rev()
        .find(|push| push.len() == 33 && matches!(push.first(), Some(0x02 | 0x03)))
}

fn parse_script_pushes(script: &[u8]) -> Option<Vec<&[u8]>> {
    let mut pushes = Vec::new();
    let mut index = 0;
    while index < script.len() {
        let opcode = script[index];
        index += 1;

        let len = match opcode {
            0x01..=0x4b => opcode as usize,
            0x4c => {
                let len = *script.get(index)? as usize;
                index += 1;
                len
            }
            0x4d => {
                let bytes = script.get(index..index + 2)?;
                index += 2;
                u16::from_le_bytes([bytes[0], bytes[1]]) as usize
            }
            _ => return None,
        };

        let data = script.get(index..index + len)?;
        pushes.push(data);
        index += len;
    }

    Some(pushes)
}

fn read_raw_transaction_for_tx(
    conn: &rusqlite::Connection,
    account_uuid: &[u8],
    txid: &[u8],
) -> Result<Option<Vec<u8>>, String> {
    conn.query_row(
        "SELECT raw FROM v_transactions \
         WHERE account_uuid = ?1 AND txid = ?2 AND raw IS NOT NULL \
         LIMIT 1",
        rusqlite::params![account_uuid, txid],
        |row| row.get(0),
    )
    .optional()
    .map_err(|e| format!("Query error: {e}"))
}

/// The account's `v_transactions` rows, or just `txid`'s.
fn read_history_bases(
    conn: &rusqlite::Connection,
    account_uuid: &[u8],
    txid: Option<&[u8]>,
) -> Result<Vec<TxBase>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT vt.txid, vt.mined_height, vt.expired_unmined, vt.expiry_height,
                    vt.account_balance_delta,
                    COALESCE(vt.fee_paid, 0), COALESCE(vt.block_time, 0),
                    COALESCE(vt.total_spent, 0), COALESCE(vt.total_received, 0),
                    COALESCE(vt.is_shielding, 0), COALESCE(vt.tx_index, -1)
             FROM v_transactions vt
             WHERE vt.account_uuid = ?1 AND (?2 IS NULL OR vt.txid = ?2)",
        )
        .map_err(|e| format!("SQL error: {e}"))?;
    let rows = stmt
        .query_map(rusqlite::params![account_uuid, txid], |row| {
            Ok(TxBase {
                txid: row.get(0)?,
                mined_height: row.get(1)?,
                expired_unmined: row.get(2)?,
                expiry_height: row.get(3)?,
                account_balance_delta: row.get(4)?,
                fee: row.get::<_, i64>(5)?.unsigned_abs(),
                block_time: row.get::<_, i64>(6)?.unsigned_abs(),
                total_spent: row.get::<_, i64>(7)?.unsigned_abs(),
                total_received: row.get::<_, i64>(8)?.unsigned_abs(),
                is_shielding: row.get(9)?,
                tx_index: row.get(10)?,
            })
        })
        .map_err(|e| format!("Query error: {e}"))?;
    rows.collect::<Result<_, _>>()
        .map_err(|e| format!("Row error: {e}"))
}

fn read_history_outputs<'a>(
    conn: &rusqlite::Connection,
    account_uuid: &[u8],
    txids: impl IntoIterator<Item = &'a [u8]>,
) -> Result<HashMap<Vec<u8>, Vec<TxOutput>>, String> {
    let mut seen = HashSet::<&[u8]>::new();
    let txid_array: Array = Rc::new(
        txids
            .into_iter()
            .filter(|txid| seen.insert(*txid))
            .map(|txid| Value::Blob(txid.to_vec()))
            .collect(),
    );
    if txid_array.is_empty() {
        return Ok(HashMap::new());
    }

    let mut stmt = conn
        .prepare(
            r#"
        SELECT
            txo.txid,
            txo.output_pool,
            txo.output_index,
            txo.from_account_uuid,
            txo.to_account_uuid,
            txo.to_address,
            txo.recipient_key_scope,
            txo.value,
            txo.memo
        FROM v_tx_outputs txo
        JOIN rarray(?2) AS active_tx ON active_tx.value = txo.txid
        WHERE txo.from_account_uuid = ?1
           OR txo.to_account_uuid = ?1
        "#,
        )
        .map_err(|e| format!("SQL error: {e}"))?;

    let rows = stmt
        .query_map(rusqlite::params![account_uuid, txid_array], |row| {
            Ok(TxOutput {
                txid: row.get(0)?,
                output_pool: row.get(1)?,
                output_index: row.get(2)?,
                from_account_uuid: row.get(3)?,
                to_account_uuid: row.get(4)?,
                to_address: row.get(5)?,
                to_key_scope: row.get(6)?,
                value: row.get::<_, i64>(7)?.unsigned_abs(),
                memo: row.get(8)?,
            })
        })
        .map_err(|e| format!("Query error: {e}"))?;

    let mut outputs = HashMap::<Vec<u8>, Vec<TxOutput>>::new();
    for row in rows {
        let output = row.map_err(|e| format!("Row error: {e}"))?;
        outputs.entry(output.txid.clone()).or_default().push(output);
    }
    Ok(outputs)
}

fn summarize_activity_outputs(
    base: &TxBase,
    outputs: &[TxOutput],
    account_uuid: &[u8],
) -> ActivitySummary {
    let mut summary = ActivitySummary::default();

    for output in outputs {
        let from_own = output.from_account_uuid.as_deref() == Some(account_uuid);
        let to_own = output.to_account_uuid.as_deref() == Some(account_uuid);

        if base.is_shielding {
            if to_own && is_shielded_pool(output.output_pool) {
                summary.shielded.add_output(output);
            }
            continue;
        }

        let visible_self_output = from_own && to_own && is_user_visible_self_output(output);
        let visible_sent = from_own && (!to_own || visible_self_output);
        let visible_received = to_own && (!from_own || visible_self_output);

        if visible_sent {
            summary.sent.add_output(output);
        }
        if visible_received {
            summary.received.add_output(output);
        }
    }

    summary
}

fn detail_includes_output(
    base: &TxBase,
    output: &TxOutput,
    account_uuid: &[u8],
    tx_kind: &str,
) -> bool {
    let from_own = output.from_account_uuid.as_deref() == Some(account_uuid);
    let to_own = output.to_account_uuid.as_deref() == Some(account_uuid);

    match tx_kind {
        "shielded" => base.is_shielding && to_own && is_shielded_pool(output.output_pool),
        "sent" => {
            !base.is_shielding && from_own && (!to_own || is_user_visible_self_output(output))
        }
        "received" | "receiving" => {
            !base.is_shielding && to_own && (!from_own || is_user_visible_self_output(output))
        }
        _ => false,
    }
}

fn is_shielded_pool(output_pool: i64) -> bool {
    matches!(output_pool, SAPLING_POOL | ORCHARD_POOL | IRONWOOD_POOL)
}

/// Self outputs are user-visible only when received at an external or foreign
/// address; internal and ephemeral receivers are change and funding mechanics.
fn is_user_visible_self_output(output: &TxOutput) -> bool {
    matches!(
        output.output_pool,
        TRANSPARENT_POOL | SAPLING_POOL | ORCHARD_POOL | IRONWOOD_POOL
    ) && matches!(output.to_key_scope, Some(0) | Some(-1))
}

fn decode_text_memo(memo: Option<&[u8]>) -> Option<String> {
    match Memo::from_bytes(memo?).ok()? {
        Memo::Text(text) => {
            let text = String::from(text);
            if text.trim().is_empty() {
                None
            } else {
                Some(text)
            }
        }
        Memo::Empty | Memo::Future(_) | Memo::Arbitrary(_) => None,
    }
}

fn classify_history_tx(base: &TxBase, summary: &ActivitySummary) -> Vec<ClassifiedTx> {
    if base.is_shielding {
        let amount = if summary.shielded.amount > 0 {
            summary.shielded.amount
        } else {
            base.total_received
        };
        return vec![build_classified_tx(base, "shielded", amount, "shielded", 0)];
    }

    let mut rows = Vec::new();
    if summary.sent.amount > 0 {
        rows.push(build_classified_tx(
            base,
            "sent",
            summary.sent.amount,
            summary.sent.display_pool(),
            1,
        ));
    }
    if summary.received.amount > 0 {
        rows.push(build_classified_tx(
            base,
            receiving_tx_kind(base),
            summary.received.amount,
            summary.received.display_pool(),
            2,
        ));
    }

    if rows.is_empty() {
        if base.mined_height.is_none() && base.account_balance_delta < 0 && base.total_spent > 0 {
            let sent_amount = base
                .account_balance_delta
                .unsigned_abs()
                .saturating_sub(base.fee);
            if sent_amount > 0 {
                rows.push(build_classified_tx(base, "sent", sent_amount, "unknown", 1));
                return rows;
            }
        }
        if base.total_spent > 0 && base.total_received > 0 {
            return rows;
        }
        if base.account_balance_delta > 0 {
            rows.push(build_classified_tx(
                base,
                receiving_tx_kind(base),
                base.account_balance_delta as u64,
                "unknown",
                2,
            ));
        } else {
            rows.push(build_classified_tx(base, "unknown", 0, "unknown", 3));
        }
    }

    rows
}

fn receiving_tx_kind(base: &TxBase) -> &'static str {
    if base.mined_height.is_none() && !base.expired_unmined {
        "receiving"
    } else {
        "received"
    }
}

fn build_classified_tx(
    base: &TxBase,
    tx_kind: &str,
    display_amount: u64,
    display_pool: &str,
    row_order: u8,
) -> ClassifiedTx {
    ClassifiedTx {
        info: TransactionInfo {
            txid: TxId::from_bytes(
                base.txid
                    .as_slice()
                    .try_into()
                    .expect("wallet DB txids are 32 bytes"),
            ),
            mined_height: base.mined_height.unwrap_or(0) as u64,
            expired_unmined: base.expired_unmined,
            fee: base.fee,
            block_time: base.block_time,
            tx_kind: tx_kind.to_string(),
            display_amount,
            display_pool: display_pool.to_string(),
        },
        sort_pending_rank: u8::from(base.mined_height.is_none() && !base.expired_unmined),
        sort_height: base
            .mined_height
            .or(base.expiry_height.filter(|_| base.expired_unmined))
            .unwrap_or(0) as u64,
        tx_index: base.tx_index,
        row_order,
    }
}

/// A wallet-created transaction that is eligible for automatic
/// resubmit: unmined, not past its expiry height or explicitly
/// no-expiry, and sending value out of the wallet.
///
/// `raw_tx` is the full serialized transaction bytes ready to feed
/// back into `send_transaction` — no re-encoding required. The
/// resubmit path at `sync::send::resubmit_pending_transactions`
/// consumes this struct directly.
pub(crate) struct ResubmittableTx {
    pub txid: TxId,
    pub raw_tx: Vec<u8>,
}

/// Returns resubmittable transactions.
pub(crate) fn get_resubmittable_txs(
    db_path: &str,
    current_height: u32,
) -> Result<Vec<ResubmittableTx>, String> {
    let conn = open_readonly_conn(db_path)?;
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT txid, raw \
             FROM v_transactions \
             WHERE mined_height IS NULL \
               AND (expiry_height = 0 OR expiry_height > ?1) \
               AND account_balance_delta < 0 \
               AND raw IS NOT NULL",
        )
        .map_err(|e| format!("SQL error: {e}"))?;
    let rows = stmt
        .query_map([current_height], |row| {
            Ok(ResubmittableTx {
                txid: TxId::from_bytes(row.get(0)?),
                raw_tx: row.get(1)?,
            })
        })
        .map_err(|e| format!("Query error: {e}"))?;
    rows.collect::<Result<_, _>>()
        .map_err(|e| format!("Row error: {e}"))
}

#[cfg(test)]
mod tests {
    //! SQL-predicate regression tests for `get_resubmittable_txs`.
    //!
    //! `get_resubmittable_txs` is a thin wrapper around a `v_transactions`
    //! SELECT, but the SELECT is the entire contract: it's the piece that
    //! encodes the four resubmit invariants.
    //!
    //! We test against a stand-in schema: a real SQLite DB with a plain
    //! `v_transactions` table mirroring the columns the production view
    //! exposes. That's enough for the WHERE clause to exercise each
    //! filter independently without standing up the whole
    //! `zcash_client_sqlite` migration stack.
    //!
    //! If the production `v_transactions` view ever gains (or loses)
    //! one of the columns we query here (`txid`, `raw`, `mined_height`,
    //! `expiry_height`, `account_balance_delta`), the real build breaks
    //! loudly at the first real query — but these unit tests still
    //! exercise the logic, so a regression in the SQL text shows up here
    //! first.
    use super::*;
    use tempfile::NamedTempFile;

    /// Build a throwaway SQLite database with a minimal
    /// `v_transactions` table and return its `NamedTempFile`
    /// handle. Tests keep the handle alive for the duration of the
    /// test so the file isn't auto-deleted under them.
    fn fresh_db() -> NamedTempFile {
        let file = NamedTempFile::new().unwrap();
        let conn = rusqlite::Connection::open(file.path()).unwrap();
        conn.execute_batch(
            "CREATE TABLE v_transactions (
                 txid BLOB NOT NULL,
                 raw BLOB,
                 mined_height INTEGER,
                 expiry_height INTEGER,
                 account_balance_delta INTEGER NOT NULL
             );",
        )
        .unwrap();
        file
    }

    /// Insert one synthetic row into `v_transactions`.
    fn insert_row(
        db: &NamedTempFile,
        txid: &[u8],
        raw: Option<&[u8]>,
        mined_height: Option<i64>,
        expiry_height: Option<i64>,
        account_balance_delta: i64,
    ) {
        let conn = rusqlite::Connection::open(db.path()).unwrap();
        conn.execute(
            "INSERT INTO v_transactions (txid, raw, mined_height, expiry_height, account_balance_delta)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![txid, raw, mined_height, expiry_height, account_balance_delta],
        )
        .unwrap();
    }

    fn fake_txid(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    fn fake_raw() -> Vec<u8> {
        vec![0xDE, 0xAD, 0xBE, 0xEF]
    }

    fn transparent_source_raw_tx() -> Vec<u8> {
        hex::decode(
            "0400008085202f8901aee37187e843da597683c26c01457f5fd3b1a038996ef74dc8d60d483aaf395a000000006b483045022100874c70db77ea9e93f75cc83a9e141e17c8eb97588e29fe4e307631fdde4f162a02203493df62d648cd86a1189eaf9bcafc652bc14c5df02519d9e45e25b32aaffb5b012102106a2dcaaac2ae3b24358a03f4264e05db420c5b090399bc23885fa02fef7716ffffffff02764e1900000000001976a914fb451987556f7a19b726966ee6cff917e0bb3bfb88ac560ca400000000001976a9141634f5ff0b8f6603a17570436d6c12a91f4b1fed88ac00000000000000000000000000000000000000",
        )
        .unwrap()
    }

    fn transparent_source_test_address() -> String {
        let pubkey =
            hex::decode("02106a2dcaaac2ae3b24358a03f4264e05db420c5b090399bc23885fa02fef7716")
                .unwrap();
        let address = TransparentAddress::PublicKeyHash(transparent::util::hash160::hash(&pubkey));
        zcash_keys::encoding::encode_transparent_address_p(&WalletNetwork::Test, &address)
    }

    fn test_account_uuid() -> uuid::Uuid {
        uuid::Uuid::from_u128(0x7e2b16db08384ddba8026fd48b9e0d02)
    }

    fn second_test_account_uuid() -> uuid::Uuid {
        uuid::Uuid::from_u128(0x3eb4ded306b74bf2a5393f1b78d792a6)
    }

    fn me() -> Vec<u8> {
        test_account_uuid().as_bytes().to_vec()
    }

    fn other() -> Vec<u8> {
        second_test_account_uuid().as_bytes().to_vec()
    }

    fn base(id: u8) -> TxBase {
        TxBase {
            txid: fake_txid(id).to_vec(),
            mined_height: Some(100),
            expired_unmined: false,
            expiry_height: None,
            account_balance_delta: 0,
            fee: 0,
            block_time: 1_000,
            total_spent: 0,
            total_received: 0,
            is_shielding: false,
            tx_index: 1,
        }
    }

    fn output(
        id: u8,
        pool: i64,
        from: Option<Vec<u8>>,
        to: Option<Vec<u8>>,
        value: u64,
    ) -> TxOutput {
        TxOutput {
            txid: fake_txid(id).to_vec(),
            output_pool: pool,
            output_index: 0,
            from_account_uuid: from,
            to_account_uuid: to,
            to_address: None,
            to_key_scope: None,
            value,
            memo: None,
        }
    }

    /// `output` from the account to itself on `scope` (0 external, 1 internal change).
    fn own(id: u8, pool: i64, value: u64, scope: i64) -> TxOutput {
        TxOutput {
            to_key_scope: Some(scope),
            ..output(id, pool, Some(me()), Some(me()), value)
        }
    }

    fn history(txs: Vec<(TxBase, Vec<TxOutput>)>, limit: Option<u32>) -> Vec<TransactionInfo> {
        let bases: Vec<_> = txs.iter().map(|(base, _)| base.clone()).collect();
        let outputs = txs.into_iter().map(|(base, o)| (base.txid, o)).collect();
        assemble_history(&bases, &outputs, &me(), limit)
    }

    /// Each history row as (kind, amount, pool).
    fn rows(history: &[TransactionInfo]) -> Vec<(&str, u64, &str)> {
        let mut rows = Vec::new();
        for tx in history {
            rows.push((
                tx.tx_kind.as_str(),
                tx.display_amount,
                tx.display_pool.as_str(),
            ));
        }
        rows
    }

    fn spend(id: u8, delta: i64, spent: u64, received: u64) -> TxBase {
        TxBase {
            account_balance_delta: delta,
            total_spent: spent,
            total_received: received,
            ..base(id)
        }
    }

    #[test]
    fn a_self_send_splits_into_sent_and_received() {
        for (pool, name) in [(0, "transparent"), (3, "shielded")] {
            let tx = spend(1, -15_000, 1_015_000, 1_000_000);
            let got = history(vec![(tx, vec![own(1, pool, 1_000_000, 0)])], None);
            assert_eq!(
                rows(&got),
                [("sent", 1_000_000, name), ("received", 1_000_000, name)]
            );
        }
    }

    #[test]
    fn a_self_send_across_pools_is_mixed_and_hides_change() {
        let tx = spend(1, -15_000, 6_115_000, 6_100_000);
        let outputs = vec![
            own(1, 0, 100_000, 0),
            own(1, 3, 1_000_000, 0),
            own(1, 3, 5_000_000, 1),
        ];
        assert_eq!(
            rows(&history(vec![(tx, outputs)], None)),
            [
                ("sent", 1_100_000, "mixed"),
                ("received", 1_100_000, "mixed")
            ]
        );
    }

    #[test]
    fn a_change_only_transaction_is_hidden() {
        let tx = spend(1, -10_000, 17_252_102, 17_242_102);
        let outputs = vec![own(1, 3, 7_242_102, 1), own(1, 3, 10_000_000, 1)];
        assert!(history(vec![(tx, outputs)], None).is_empty());
    }

    #[test]
    fn a_shielding_transaction_is_one_shielded_row() {
        let tx = TxBase {
            is_shielding: true,
            ..spend(1, -10_000, 10_010_000, 10_000_000)
        };
        assert_eq!(
            rows(&history(vec![(tx, vec![own(1, 3, 10_000_000, 0)])], None)),
            [("shielded", 10_000_000, "shielded")]
        );
    }

    #[test]
    fn a_send_to_another_local_account_is_sent_and_the_reverse_is_received() {
        let sent = (
            spend(1, -5_000_000, 5_000_000, 0),
            vec![output(1, 0, Some(me()), Some(other()), 5_000_000)],
        );
        let received = (
            spend(1, 5_000_000, 0, 5_000_000),
            vec![output(1, 0, Some(other()), Some(me()), 5_000_000)],
        );
        assert_eq!(
            rows(&history(vec![sent], None)),
            [("sent", 5_000_000, "transparent")]
        );
        assert_eq!(
            rows(&history(vec![received], None)),
            [("received", 5_000_000, "transparent")]
        );
    }

    #[test]
    fn a_transparent_send_excludes_its_ephemeral_and_shielded_change_outputs() {
        let tx = spend(1, -15_000, 1_265_000, 1_150_000);
        let outputs = vec![
            own(1, 0, 150_000, 2),
            own(1, 3, 1_000_000, 1),
            output(1, 0, Some(me()), None, 100_000),
        ];
        assert_eq!(
            rows(&history(vec![(tx, outputs)], None)),
            [("sent", 100_000, "transparent")]
        );
    }

    #[test]
    fn a_pending_send_without_output_metadata_shows_the_spent_amount_less_the_fee() {
        let tx = TxBase {
            mined_height: None,
            fee: 10_000,
            ..spend(1, -10_010_000, 10_010_000, 9_000_000)
        };
        assert_eq!(
            rows(&history(vec![(tx, vec![own(1, 3, 9_000_000, 1)])], None)),
            [("sent", 10_000_000, "unknown")]
        );
    }

    #[test]
    fn pending_rows_come_first_and_the_limit_applies_after_sorting() {
        let confirmed = (
            spend(1, 1_000_000, 0, 1_000_000),
            vec![output(1, 3, None, Some(me()), 1_000_000)],
        );
        let receiving = TxBase {
            mined_height: None,
            ..spend(2, 2_000_000, 0, 2_000_000)
        };
        let receiving = (receiving, vec![output(2, 3, None, Some(me()), 2_000_000)]);
        let sending = TxBase {
            mined_height: None,
            ..spend(3, -1_010_000, 1_010_000, 0)
        };
        let sending = (sending, vec![output(3, 3, Some(me()), None, 1_000_000)]);

        let all = history(
            vec![confirmed.clone(), receiving.clone(), sending.clone()],
            None,
        );
        assert_eq!(all.len(), 3);
        assert_eq!(all[2].tx_kind, "received");
        assert_eq!(
            rows(&history(vec![confirmed, receiving], Some(1))),
            [("receiving", 2_000_000, "shielded")]
        );
        assert_eq!(
            rows(&history(vec![sending], Some(1))),
            [("sent", 1_000_000, "shielded")]
        );
    }

    #[test]
    fn rows_sort_by_height_and_an_expired_send_stays_at_its_expiry_height() {
        let expired = TxBase {
            mined_height: None,
            expired_unmined: true,
            expiry_height: Some(150),
            block_time: 0,
            ..spend(1, -10_010_000, 10_010_000, 0)
        };
        let expired = (expired, vec![output(1, 0, Some(me()), None, 10_000_000)]);
        let newer = TxBase {
            block_time: 0,
            ..spend(2, -40_000, 17_040_000, 17_000_000)
        };
        let newer = (newer, vec![own(2, 0, 17_000_000, 0)]);

        let all = history(vec![expired.clone(), newer.clone()], None);
        assert_eq!(
            all.iter()
                .map(|tx| (tx.txid, tx.tx_kind.as_str()))
                .collect::<Vec<_>>(),
            [
                (TxId::from_bytes(fake_txid(1)), "sent"),
                (TxId::from_bytes(fake_txid(2)), "sent"),
                (TxId::from_bytes(fake_txid(2)), "received"),
            ]
        );
        assert!(all[0].expired_unmined);
        assert_eq!(rows(&history(vec![expired, newer], Some(1))).len(), 1);
    }

    fn detail_of(
        base: &TxBase,
        outputs: &[TxOutput],
        kind: &str,
    ) -> Vec<(Option<String>, u64, Option<String>)> {
        let included = |o: &&TxOutput| detail_includes_output(base, o, &me(), kind);
        let row = |o: &TxOutput| {
            (
                o.to_address.clone(),
                o.value,
                decode_text_memo(o.memo.as_deref()),
            )
        };
        outputs.iter().filter(included).map(row).collect()
    }

    fn with_memo(output: TxOutput, address: &str, memo: &[u8]) -> TxOutput {
        TxOutput {
            to_address: Some(address.into()),
            memo: Some(memo.to_vec()),
            ..output
        }
    }

    #[test]
    fn a_sent_detail_shows_the_recipient_and_memo_but_not_change() {
        let tx = spend(1, -1_010_000, 1_010_000, 0);
        let outputs = [
            with_memo(
                output(1, 3, Some(me()), None, 1_000_000),
                "u-recipient",
                b"hello",
            ),
            with_memo(own(1, 3, 5_000, 1), "u-change", &[0xF6]),
        ];
        assert_eq!(
            detail_of(&tx, &outputs, "sent"),
            [(Some("u-recipient".into()), 1_000_000, Some("hello".into()))]
        );
    }

    #[test]
    fn a_received_detail_shows_every_memo_and_the_source_comes_from_the_raw_tx() {
        let tx = spend(1, 3_000_000, 0, 3_000_000);
        let outputs = [
            with_memo(
                output(1, 3, None, Some(me()), 1_000_000),
                "u-mine",
                b"first memo",
            ),
            with_memo(
                output(1, 3, None, Some(me()), 2_000_000),
                "u-mine",
                b"other memo",
            ),
        ];
        for kind in ["received", "receiving"] {
            let memos: Vec<_> = detail_of(&tx, &outputs, kind)
                .into_iter()
                .map(|row| row.2)
                .collect();
            assert_eq!(
                memos,
                [Some("first memo".into()), Some("other memo".into())]
            );
        }

        let expected = transparent_source_test_address();
        let raw = transparent_source_raw_tx();
        assert_eq!(
            received_source_from_raw_transaction(WalletNetwork::Test, None),
            None
        );
        assert_eq!(
            received_source_from_raw_transaction(WalletNetwork::Test, Some(&raw)),
            Some(expected)
        );
    }

    #[test]
    fn a_self_send_detail_has_the_same_visible_output_under_sent_and_received() {
        let tx = spend(1, -10_000, 1_010_000, 1_000_000);
        let outputs = [
            with_memo(own(1, 3, 1_000_000, 0), "u-self", b"self memo"),
            with_memo(own(1, 3, 500_000, 1), "u-change", &[0xF6]),
        ];
        for kind in ["sent", "received"] {
            assert_eq!(
                detail_of(&tx, &outputs, kind),
                [(Some("u-self".into()), 1_000_000, Some("self memo".into()))]
            );
        }
        assert!(detail_of(&tx, &outputs, "shielded").is_empty());
    }

    #[test]
    fn a_shielding_detail_shows_its_shielded_outputs_only_under_shielded() {
        let tx = TxBase {
            is_shielding: true,
            ..spend(1, -10_000, 1_010_000, 1_000_000)
        };
        let outputs = [with_memo(own(1, 3, 1_000_000, 0), "u-shielded", &[0xF6])];
        assert_eq!(
            detail_of(&tx, &outputs, "shielded"),
            [(Some("u-shielded".into()), 1_000_000, None)]
        );
        assert!(detail_of(&tx, &outputs, "sent").is_empty());
    }

    #[test]
    fn every_query_prepares_against_the_real_wallet_schema() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("wallet.db");
        let db = db.to_str().unwrap();
        let seed = crate::keys::mnemonic_to_seed(&crate::keys::generate_mnemonic()).unwrap();
        let uuid = crate::account::init_db_and_create_account(
            db,
            WalletNetwork::Regtest,
            &seed,
            None,
            "A",
        )
        .unwrap()
        .uuid;
        let wallet_account = Account {
            uuid: uuid.clone(),
            name: "A".into(),
            network: WalletNetwork::Regtest,
            db_path: db.into(),
        };
        let account = uuid::Uuid::parse_str(&uuid).unwrap();
        let (account, txid) = (account.as_bytes().as_slice(), fake_txid(1));

        assert!(get_transaction_history(&wallet_account, None)
            .unwrap()
            .is_empty());
        let conn = open_readonly_conn(db).unwrap();
        assert!(read_history_outputs(&conn, account, [txid.as_slice()])
            .unwrap()
            .is_empty());
        assert!(read_raw_transaction_for_tx(&conn, account, &txid)
            .unwrap()
            .is_none());
        let missing = get_transaction_detail(&wallet_account, &TxId::from_bytes(txid), "sent");
        assert_eq!(missing.err().as_deref(), Some("Transaction not found"));
        assert!(get_resubmittable_txs(db, 1).unwrap().is_empty());
    }

    #[test]
    fn resubmit_excludes_mined_txs() {
        // A tx with `mined_height IS NOT NULL` is already on-chain;
        // resubmitting would be pointless at best and could surface a
        // confusing rejection from lightwalletd.
        let db = fresh_db();
        insert_row(
            &db,
            &fake_txid(0x01),
            Some(&fake_raw()),
            Some(1_000_000), // mined_height set → mined
            Some(1_000_100),
            -5_000,
        );
        let got = get_resubmittable_txs(db.path().to_str().unwrap(), 900_000).unwrap();
        assert!(
            got.is_empty(),
            "mined tx must not be a resubmit candidate, got {got:?}",
            got = got.len(),
        );
    }

    #[test]
    fn resubmit_excludes_expired_txs() {
        // `expiry_height > current_height` is the network's
        // still-relayable check for expiring transactions. A tx whose
        // expiry equals the current height is already past the window.
        let db = fresh_db();
        insert_row(
            &db,
            &fake_txid(0x02),
            Some(&fake_raw()),
            None,
            Some(1_000_000), // expiry == current → expired
            -5_000,
        );
        let got = get_resubmittable_txs(db.path().to_str().unwrap(), 1_000_000).unwrap();
        assert!(got.is_empty(), "tx with expiry==current must be excluded");

        // Walk the boundary: one block below current is definitely expired.
        let db2 = fresh_db();
        insert_row(
            &db2,
            &fake_txid(0x02),
            Some(&fake_raw()),
            None,
            Some(999_999),
            -5_000,
        );
        let got2 = get_resubmittable_txs(db2.path().to_str().unwrap(), 1_000_000).unwrap();
        assert!(got2.is_empty(), "tx with expiry<current must be excluded");
    }

    #[test]
    fn resubmit_excludes_received_txs() {
        // `account_balance_delta >= 0` means the account gained or broke
        // even on this tx — it's an incoming transfer we just happened to
        // have raw bytes for (e.g. re-read from lightwalletd during a
        // rescan). Resubmitting "our" received txs back to the network
        // would be meaningless.
        let db = fresh_db();
        insert_row(
            &db,
            &fake_txid(0x03),
            Some(&fake_raw()),
            None,
            Some(1_000_100),
            5_000, // positive delta → inbound
        );
        let got = get_resubmittable_txs(db.path().to_str().unwrap(), 1_000_000).unwrap();
        assert!(got.is_empty(), "received-only tx must be excluded");

        // Also the zero case: a break-even tx shouldn't show up either
        // (it's still not an "outbound" the wallet needs to keep alive).
        let db2 = fresh_db();
        insert_row(
            &db2,
            &fake_txid(0x03),
            Some(&fake_raw()),
            None,
            Some(1_000_100),
            0,
        );
        let got2 = get_resubmittable_txs(db2.path().to_str().unwrap(), 1_000_000).unwrap();
        assert!(got2.is_empty(), "zero-delta tx must be excluded");
    }

    #[test]
    fn resubmit_excludes_raw_null_txs() {
        // `raw IS NULL` means we don't have bytes to broadcast. This row
        // exists because sync learned about the tx via decrypt-and-store
        // without the raw bundle, and there's nothing we can resubmit.
        let db = fresh_db();
        insert_row(
            &db,
            &fake_txid(0x04),
            None, // raw NULL
            None,
            Some(1_000_100),
            -5_000,
        );
        let got = get_resubmittable_txs(db.path().to_str().unwrap(), 1_000_000).unwrap();
        assert!(got.is_empty(), "raw-null tx must be excluded");
    }

    #[test]
    fn resubmit_includes_valid_outbound_pending() {
        // The happy path: unmined, inside expiry, outbound, raw present.
        let db = fresh_db();
        let txid = fake_txid(0x05);
        let raw = fake_raw();
        insert_row(&db, &txid, Some(&raw), None, Some(1_000_100), -5_000);
        let got = get_resubmittable_txs(db.path().to_str().unwrap(), 1_000_000).unwrap();
        assert_eq!(got.len(), 1, "outbound pending tx must appear exactly once");
        assert_eq!(got[0].txid, TxId::from_bytes(txid));
        assert_eq!(got[0].raw_tx, raw);
    }

    #[test]
    fn resubmit_includes_no_expiry_outbound_pending() {
        // Expiry height 0 is the protocol no-expiry marker, so these
        // transactions must keep participating in auto-resubmit across
        // restarts and long migration windows.
        let db = fresh_db();
        let txid = fake_txid(0x07);
        let raw = fake_raw();
        insert_row(&db, &txid, Some(&raw), None, Some(0), -5_000);

        let got = get_resubmittable_txs(db.path().to_str().unwrap(), 1_000_000).unwrap();
        assert_eq!(
            got.len(),
            1,
            "no-expiry outbound pending tx must be resubmittable"
        );
        assert_eq!(got[0].txid, TxId::from_bytes(txid));
        assert_eq!(got[0].raw_tx, raw);
    }

    #[test]
    fn resubmit_dedupes_multi_account_rows() {
        // A tx that touches two of the wallet's own accounts shows up as
        // two rows in `v_transactions`. `SELECT DISTINCT txid, raw,
        // expiry_height` should collapse that to one broadcast — double-
        // sending identical bytes would be a regression.
        let db = fresh_db();
        let txid = fake_txid(0x06);
        let raw = fake_raw();
        // Two rows for the same tx, different account-level deltas, both
        // still outbound at the row level (account-internal transfer with
        // a net negative for both of the wallet's participating accounts
        // after fees — contrived but possible).
        insert_row(&db, &txid, Some(&raw), None, Some(1_000_100), -3_000);
        insert_row(&db, &txid, Some(&raw), None, Some(1_000_100), -2_000);

        let got = get_resubmittable_txs(db.path().to_str().unwrap(), 1_000_000).unwrap();
        assert_eq!(
            got.len(),
            1,
            "multi-account rows with same (txid, raw, expiry) must dedupe",
        );
    }

    #[test]
    fn resubmit_returns_empty_when_table_empty() {
        // Baseline: an empty table must return `Ok(vec![])`, not an
        // error. `resubmit_pending_transactions` relies on this to
        // decide the "nothing to do" case.
        let db = fresh_db();
        let got = get_resubmittable_txs(db.path().to_str().unwrap(), 1_000_000).unwrap();
        assert!(got.is_empty());
    }
}
