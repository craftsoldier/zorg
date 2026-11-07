//! Transaction enhancement pass for the sync engine.
//!
//! `scan_cached_blocks` walks compact blocks and discovers transactions
//! that are relevant to the wallet, but a compact block only carries
//! the subset of transaction data needed for shielded-note discovery.
//! Things the wallet still has to learn afterwards:
//!
//!   - The full transaction bytes (for memo decryption, transparent
//!     input/output tracking, etc.).
//!   - Mined status for a transaction the wallet knows about but
//!     hasn't confirmed on-chain yet.
//!   - Transparent-address history in a given block range (used when
//!     the wallet imports or derives a new t-address and has to
//!     backfill its activity).
//!
//! Librustzcash signals these gaps by populating
//! `db.transaction_data_requests()`. This module services the queue
//! against lightwalletd via three gRPC calls (`GetTransaction`,
//! `TransactionsInvolvingAddress`) and writes the results back into
//! `db` using `decrypt_and_store_transaction` and
//! `set_transaction_status`. The loop retries up to three times
//! because servicing one request can legally populate new requests
//! (e.g. a newly-decrypted transaction may reveal additional parent
//! transactions to enhance).

use std::collections::HashSet;

use tonic::{transport::Channel, Code, Status};
use zcash_client_backend::{
    data_api::{
        wallet::decrypt_and_store_transaction, TransactionDataRequest, TransactionStatus,
        WalletRead, WalletWrite,
    },
    proto::service::{compact_tx_streamer_client::CompactTxStreamerClient, RawTransaction},
};
use zcash_primitives::transaction::Transaction;
use zcash_protocol::{
    consensus::{BlockHeight, BranchId},
    TxId,
};

use crate::db::with_wallet_db_write_lock;

use super::{lwd, SyncError, WalletDatabase};

/// Services `db.transaction_data_requests()` against lightwalletd until
/// the queue is empty or no request is actionable. Returns `SyncError::Db`
/// if `db.transaction_data_requests()` itself fails.
/// Per-request failures are split by semantics: an explicit
/// "txid not recognized" response is recorded via
/// `set_transaction_status` so it doesn't get retried forever, while
/// transient network failures bubble up as `SyncError::Network` so the
/// outer sync retry path can recover without deleting the request.
pub(super) async fn run_enhancement(
    client: &mut CompactTxStreamerClient<Channel>,
    db: &mut WalletDatabase,
) -> Result<(), SyncError> {
    let network = *db.params();
    let mut failed_txids: HashSet<TxId> = HashSet::new();

    for _ in 0..3 {
        let requests = db
            .transaction_data_requests()
            .map_err(|e| SyncError::db(format!("transaction_data_requests: {e}")))?;
        if requests.is_empty() {
            break;
        }

        // If nothing in the queue is actionable (e.g. address-scoped
        // requests without an `end` height, which we can't service
        // without synthesizing a range), break rather than looping
        // forever on the same inert queue.
        let actionable = requests.iter().any(request_is_actionable);
        if !actionable {
            break;
        }

        for req in &requests {
            match req {
                TransactionDataRequest::GetStatus(txid)
                | TransactionDataRequest::Enhancement(txid) => {
                    if failed_txids.contains(txid) {
                        continue;
                    }

                    match lwd::get_transaction(client, txid.as_ref().to_vec()).await {
                        Ok(raw) => {
                            if !raw.data.is_empty() {
                                if let Err(e) = store_transaction(db, &raw) {
                                    log::warn!("sync: could not store {txid}: {e}");
                                }
                            }
                            if matches!(req, TransactionDataRequest::GetStatus(_)) {
                                let status = transaction_status_from_raw_height(raw.height)?;
                                let txid = *txid;
                                if let Err(e) = with_wallet_db_write_lock(
                                    "sync_engine.enhance.set_transaction_status",
                                    || db.set_transaction_status(txid, status),
                                ) {
                                    log::error!("sync: set_transaction_status failed: {e}");
                                }
                            }
                        }
                        Err(e) => match classify_get_transaction_error(&e) {
                            GetTransactionErrorAction::MarkTxidNotRecognized => {
                                log::warn!("sync: get_transaction did not recognize {txid}: {e}");
                                failed_txids.insert(*txid);
                                let txid = *txid;
                                if let Err(e) = with_wallet_db_write_lock(
                                    "sync_engine.enhance.set_transaction_status",
                                    || {
                                        db.set_transaction_status(
                                            txid,
                                            TransactionStatus::TxidNotRecognized,
                                        )
                                    },
                                ) {
                                    log::error!("sync: set_transaction_status failed: {e}");
                                }
                            }
                            GetTransactionErrorAction::RetryAsNetwork => {
                                return Err(SyncError::net(format!(
                                    "get_transaction failed for {txid}: {e}"
                                )));
                            }
                        },
                    }
                }
                TransactionDataRequest::TransactionsInvolvingAddress(req) => {
                    let end_height = match req.block_range_end() {
                        Some(h) => h,
                        None => continue,
                    };
                    let addr_str = zcash_keys::encoding::encode_transparent_address_p(
                        &network,
                        &req.address(),
                    );
                    let start = u32::from(req.block_range_start()) as u64;
                    let end = u32::from(end_height) as u64;

                    let mut stream =
                        lwd::get_taddress_txids(client, addr_str, start, end.saturating_sub(1))
                            .await?;
                    while let Some(raw) =
                        lwd::next_stream_message(&mut stream, "get_taddress_txids stream").await?
                    {
                        if !raw.data.is_empty() {
                            if let Err(e) = store_transaction(db, &raw) {
                                log::warn!("sync: could not store transaction: {e}");
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Stores the transaction in `raw` with the wallet, at the height lightwalletd
/// reports for it.
pub(super) fn store_transaction(
    db: &mut WalletDatabase,
    raw: &RawTransaction,
) -> Result<(), SyncError> {
    let tx = Transaction::read(&raw.data[..], BranchId::Sapling)
        .map_err(|e| SyncError::parse(format!("read transaction: {e}")))?;
    let mined_height = mined_height_from_raw_height(raw.height)?;
    let network = *db.params();
    with_wallet_db_write_lock("sync_engine.decrypt_and_store_transaction", || {
        decrypt_and_store_transaction(&network, db, &tx, mined_height)
            .map_err(|e| SyncError::db(format!("decrypt_and_store_transaction: {e}")))
    })
}

/// Whether servicing `request` can make progress right now. Transaction
/// requests always can; address-scoped requests need a bounded block range.
fn request_is_actionable(request: &TransactionDataRequest) -> bool {
    match request {
        TransactionDataRequest::Enhancement(_) | TransactionDataRequest::GetStatus(_) => true,
        TransactionDataRequest::TransactionsInvolvingAddress(req) => {
            req.block_range_end().is_some()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GetTransactionErrorAction {
    MarkTxidNotRecognized,
    RetryAsNetwork,
}

fn classify_get_transaction_error(status: &Status) -> GetTransactionErrorAction {
    match status.code() {
        Code::NotFound => GetTransactionErrorAction::MarkTxidNotRecognized,
        _ => GetTransactionErrorAction::RetryAsNetwork,
    }
}

fn mined_height_from_raw_height(raw_height: u64) -> Result<Option<BlockHeight>, SyncError> {
    match raw_height {
        0 | u64::MAX => Ok(None),
        h if h <= u32::MAX as u64 => Ok(Some(BlockHeight::from_u32(h as u32))),
        h => Err(SyncError::parse(format!(
            "raw transaction height out of range: {h}"
        ))),
    }
}

fn transaction_status_from_raw_height(raw_height: u64) -> Result<TransactionStatus, SyncError> {
    mined_height_from_raw_height(raw_height).map(|height| match height {
        Some(height) => TransactionStatus::Mined(height),
        None => TransactionStatus::NotInMainChain,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::WalletNetwork;

    #[test]
    fn backend_recovers_fees_for_wallet_owned_transparent_inputs() {
        use crate::db::SYNC_DB_BUSY_TIMEOUT;
        use transparent::{
            address::{Script, TransparentAddress},
            bundle::{Authorized as TransparentAuthorized, Bundle, OutPoint, TxIn, TxOut},
            keys::IncomingViewingKey,
        };
        use zcash_keys::keys::UnifiedSpendingKey;
        use zcash_primitives::transaction::{Authorized, TransactionData, TxVersion};
        use zcash_protocol::value::Zatoshis;

        // Synthetic transactions exercise wallet ingestion, not consensus validation.
        let make_tx = |prevout: OutPoint, address: TransparentAddress, amount: u64| {
            TransactionData::<Authorized>::from_parts(
                TxVersion::V5,
                BranchId::Nu5,
                0,
                BlockHeight::from_u32(600),
                Some(Bundle {
                    vin: vec![TxIn::from_parts(
                        prevout,
                        Script::read(&[0][..]).unwrap(),
                        u32::MAX,
                    )],
                    vout: vec![TxOut::new(
                        Zatoshis::from_u64(amount).unwrap(),
                        Script::from(address.script()),
                    )],
                    authorization: TransparentAuthorized,
                }),
                None,
                None,
                None,
            )
            .freeze()
            .unwrap()
        };

        for wallet_owned_input in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("wallet.db");
            let db_path = path.to_str().unwrap();
            let network = WalletNetwork::Regtest;
            let seed = secrecy::SecretVec::new(vec![7; 64]);
            crate::account::init_db_and_create_account(db_path, network, &seed, None, "Comparison")
                .unwrap();
            let usk = UnifiedSpendingKey::from_seed(&network, &[7; 64], 0u32.try_into().unwrap())
                .unwrap();
            let (wallet_address, _) = usk
                .to_unified_full_viewing_key()
                .p2pkh()
                .unwrap()
                .derive_external_ivk()
                .unwrap()
                .default_address();
            let parent_address = if wallet_owned_input {
                wallet_address
            } else {
                TransparentAddress::PublicKeyHash([42; 20])
            };
            let parent = make_tx(OutPoint::new([9; 32], 0), parent_address, 100_000_000);
            let prevout = OutPoint::new(*parent.txid().as_ref(), 0);
            let child = make_tx(prevout, wallet_address, 99_990_000);
            let mut db =
                crate::db::open_wallet_db_with_timeout(db_path, network, SYNC_DB_BUSY_TIMEOUT)
                    .unwrap();
            db.update_chain_tip(BlockHeight::from_u32(510)).unwrap();

            // Service the child first, then the parent, as an enhancement queue can.
            decrypt_and_store_transaction(
                &network,
                &mut db,
                &child,
                Some(BlockHeight::from_u32(501)),
            )
            .unwrap();
            let read_fee = || -> Option<u64> {
                rusqlite::Connection::open(db_path)
                    .unwrap()
                    .query_row(
                        "SELECT fee FROM transactions WHERE txid = ?1",
                        [child.txid().as_ref()],
                        |row| row.get(0),
                    )
                    .unwrap()
            };
            assert_eq!(read_fee(), None);
            assert!(db
                .transaction_data_requests()
                .unwrap()
                .contains(&TransactionDataRequest::Enhancement(parent.txid())));
            decrypt_and_store_transaction(
                &network,
                &mut db,
                &parent,
                Some(BlockHeight::from_u32(500)),
            )
            .unwrap();
            let backend_fee = read_fee();
            decrypt_and_store_transaction(
                &network,
                &mut db,
                &child,
                Some(BlockHeight::from_u32(501)),
            )
            .unwrap();
            assert_eq!(read_fee(), backend_fee);
            assert_eq!(backend_fee, wallet_owned_input.then_some(10_000));
            assert_eq!(
                db.get_transaction(parent.txid()).unwrap().is_some(),
                wallet_owned_input
            );
        }
    }

    #[test]
    fn get_transaction_not_found_marks_txid_not_recognized() {
        let status = Status::new(Code::NotFound, "txid not recognized");

        assert_eq!(
            classify_get_transaction_error(&status),
            GetTransactionErrorAction::MarkTxidNotRecognized,
        );
    }

    #[test]
    fn get_transaction_transient_errors_retry_as_network() {
        for code in [
            Code::Unavailable,
            Code::DeadlineExceeded,
            Code::Cancelled,
            Code::Unknown,
            Code::Internal,
        ] {
            let status = Status::new(code, "temporary failure");
            assert_eq!(
                classify_get_transaction_error(&status),
                GetTransactionErrorAction::RetryAsNetwork,
            );
        }
    }

    #[test]
    fn raw_height_zero_and_fork_sentinel_are_not_main_chain() {
        assert_eq!(
            transaction_status_from_raw_height(0).unwrap(),
            TransactionStatus::NotInMainChain,
        );
        assert_eq!(
            transaction_status_from_raw_height(u64::MAX).unwrap(),
            TransactionStatus::NotInMainChain,
        );
    }

    #[test]
    fn raw_height_nonzero_non_sentinel_is_mined() {
        match transaction_status_from_raw_height(1_234_567).unwrap() {
            TransactionStatus::Mined(height) => {
                assert_eq!(u32::from(height), 1_234_567);
            }
            other => panic!("expected mined status, got {other:?}"),
        }
    }

    #[test]
    fn raw_height_out_of_u32_range_is_parse_error() {
        assert!(matches!(
            mined_height_from_raw_height(u32::MAX as u64 + 1),
            Err(SyncError::Parse(_)),
        ));
    }
}
