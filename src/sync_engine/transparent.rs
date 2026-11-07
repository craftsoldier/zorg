use tonic::transport::Channel;
use zcash_client_backend::{
    data_api::{WalletRead, WalletWrite},
    proto::service::compact_tx_streamer_client::CompactTxStreamerClient,
    wallet::WalletTransparentOutput,
};
use zcash_keys::encoding::AddressCodec as _;
use zcash_protocol::{consensus::BlockHeight, value::Zatoshis, TxId};

use ::transparent::{
    address::Script,
    bundle::{OutPoint, TxOut},
};

use crate::db::with_wallet_db_write_lock;

use super::enhance::store_transaction;
use super::lwd::{self, get_address_utxos_stream, next_stream_message};
use super::{SyncError, WalletDatabase};

pub(super) async fn refresh_utxos(
    client: &mut CompactTxStreamerClient<Channel>,
    db: &mut WalletDatabase,
) -> Result<(), SyncError> {
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
            .map(|addr| addr.encode(db.params()))
            .collect();

        if addresses.is_empty() {
            continue;
        }

        log::info!(
            "sync: refreshing transparent UTXOs from height {} ({} addresses)",
            u32::from(start_height),
            addresses.len(),
        );

        let mut stream = get_address_utxos_stream(client, addresses, start_height).await?;

        while let Some(reply) = next_stream_message(&mut stream, "get_address_utxos_stream").await?
        {
            let txid = TxId::from_bytes(
                reply
                    .txid
                    .try_into()
                    .map_err(|_| SyncError::parse("transparent UTXO txid was not 32 bytes"))?,
            );
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
                OutPoint::new(*txid.as_ref(), index),
                TxOut::new(value, script),
                Some(BlockHeight::from_u32(height)),
                None,
                None,
                None,
            )
            .ok_or_else(|| {
                SyncError::parse("transparent UTXO script did not decode to a wallet address")
            })?;

            // Storing the full transaction tells the wallet whether the
            // output is coinbase, which cannot be spent until it matures.
            let known = db
                .get_transaction(txid)
                .map_err(|e| SyncError::db(format!("get_transaction: {e}")))?;
            if known.is_none() {
                let raw = lwd::get_transaction(client, txid.as_ref().to_vec())
                    .await
                    .map_err(|e| SyncError::net(format!("get_transaction {txid}: {e}")))?;
                store_transaction(db, &raw)?;
            }

            with_wallet_db_write_lock("sync_engine.put_received_transparent_utxo", || {
                db.put_received_transparent_utxo(&output)
                    .map_err(|e| SyncError::db(format!("put_received_transparent_utxo: {e}")))
            })?;
        }
    }
    Ok(())
}
