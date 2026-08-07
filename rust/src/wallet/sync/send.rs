//! Software-wallet send, shield, and resubmit flows.
//!
//! Uses `zcash_client_backend`'s `propose_transfer` + `create_proposed_transactions`
//! pipeline. No PCZT, no migration, no hardware wallet.

use std::collections::HashSet;
use std::num::NonZeroUsize;

use rand::rngs::OsRng;
use rand::Rng;
use secrecy::{ExposeSecret, SecretVec};

use zcash_client_backend::data_api::wallet::input_selection::{
    GreedyInputSelector, InputSelector, LockedInputPolicy, SpendPolicy,
};
use zcash_client_backend::{
    data_api::{
        self,
        wallet::{
            self, create_proposed_transactions, propose_send_max_transfer,
            ConfirmationsPolicy, SpendingKeys,
        },
        Account as _, InputSource, MaxSpendMode, OutputLockStore, WalletCommitmentTrees, WalletRead,
    },
    fees::{zip317::MultiOutputChangeStrategy, DustOutputPolicy, SplitPolicy, StandardFeeRule},
    wallet::{LockOwner, OvkPolicy},
    zip321::{Payment, TransactionRequest},
};
use zcash_client_sqlite::{AccountUuid, ReceivedNoteId};
use zcash_keys::keys::UnifiedSpendingKey;
use zcash_primitives::transaction::{fees::FeeRule, TxId};
use zcash_protocol::{
    consensus::{self, BlockHeight, Parameters},
    memo::{Memo, MemoBytes},
    value::Zatoshis,
    PoolType, ShieldedPool,
};

use crate::wallet::db::{with_wallet_db_write_lock, WalletDatabase};
use crate::wallet::keys::parse_account_uuid;
use crate::wallet::network::WalletNetwork;
use crate::wallet::sync::{
    consume_stored_proposal, finish_stored_proposal, open_readonly_conn, open_wallet_db,
    stored_proposal_lock, StoredProposal, StoredProposalLock, PROPOSAL_STORE,
};

pub type WalletFeeRule = StandardFeeRule;

const SEND_PROPOSAL_LOCK_BLOCKS: u32 = 40;

// ======================== Result Structs ========================

pub struct ProposalResult {
    pub proposal_id: u64,
    pub needs_sapling_params: bool,
    pub fee_zatoshi: u64,
}

pub struct ExecuteProposalResult {
    pub txids: Vec<TxId>,
    pub status: String,
    pub broadcasted_count: u32,
    pub total_count: u32,
    pub message: Option<String>,
}

pub(crate) struct SendMaxEstimateResult {
    pub amount_zatoshi: u64,
    pub fee_zatoshi: u64,
    pub needs_sapling_params: bool,
}

pub(crate) struct ShieldTransparentResult {
    pub txids: String,
    pub status: String,
    pub broadcasted_count: u32,
    pub total_count: u32,
    pub message: Option<String>,
    pub fee_zatoshi: u64,
    pub shielded_zatoshi: u64,
}

pub(crate) struct ShieldTransparentStatus {
    pub can_shield: bool,
    pub fee_zatoshi: u64,
    pub shielded_zatoshi: u64,
    pub reason: String,
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ResubmitStats {
    pub attempted: usize,
    pub succeeded: usize,
    pub failed: usize,
}

// ======================== No-op Sapling Provers ========================
// Used for Orchard-only transactions where Sapling params are not available.
// Copied from the original send.rs — implements sapling_crypto::prover traits.

use sapling_crypto::{
    bundle::GrothProofBytes,
    circuit,
    keys::EphemeralSecretKey,
    prover::{OutputProver, SpendProver},
    value::{NoteValue, ValueCommitTrapdoor},
    Diversifier, MerklePath, PaymentAddress, ProofGenerationKey, Rseed,
};

const GROTH_PROOF_SIZE: usize = 192;

struct NoOpSpendProver;

impl SpendProver for NoOpSpendProver {
    type Proof = GrothProofBytes;
    fn prepare_circuit(
        _proof_generation_key: ProofGenerationKey,
        _diversifier: Diversifier,
        _rseed: Rseed,
        _value: NoteValue,
        _alpha: jubjub::Fr,
        _rcv: ValueCommitTrapdoor,
        _anchor: bls12_381::Scalar,
        _merkle_path: MerklePath,
    ) -> Option<circuit::Spend> {
        log::error!("NoOpSpendProver::prepare_circuit called — unexpected Sapling spend");
        None
    }
    fn create_proof<R: rand_core::RngCore>(&self, _c: circuit::Spend, _rng: &mut R) -> Self::Proof {
        [0u8; GROTH_PROOF_SIZE]
    }
    fn encode_proof(_p: Self::Proof) -> GrothProofBytes { [0u8; GROTH_PROOF_SIZE] }
}

struct NoOpOutputProver;

impl OutputProver for NoOpOutputProver {
    type Proof = GrothProofBytes;
    fn prepare_circuit(
        _esk: &EphemeralSecretKey,
        _pa: PaymentAddress,
        _rcm: jubjub::Fr,
        _v: NoteValue,
        _rcv: ValueCommitTrapdoor,
    ) -> circuit::Output {
        log::error!("NoOpOutputProver::prepare_circuit called — unexpected Sapling output");
        circuit::Output {
            value_commitment_opening: None,
            payment_address: None,
            commitment_randomness: None,
            esk: None,
        }
    }
    fn create_proof<R: rand_core::RngCore>(&self, _c: circuit::Output, _rng: &mut R) -> Self::Proof {
        [0u8; GROTH_PROOF_SIZE]
    }
    fn encode_proof(_p: Self::Proof) -> GrothProofBytes { [0u8; GROTH_PROOF_SIZE] }
}

// ======================== Helpers ========================

fn zip317_helper() -> (
    MultiOutputChangeStrategy<WalletFeeRule, WalletDatabase>,
    GreedyInputSelector<WalletDatabase>,
) {
    let change_strategy = MultiOutputChangeStrategy::new(
        StandardFeeRule::Zip317,
        None,
        ShieldedPool::Orchard,
        DustOutputPolicy::default(),
        SplitPolicy::with_min_output_value(
            NonZeroUsize::new(4).unwrap(),
            Zatoshis::const_from_u64(10_000_000),
        ),
    );
    (change_strategy, GreedyInputSelector::new())
}

fn build_send_request(to_address: &str, amount_zatoshi: u64, memo_str: Option<&str>) -> Result<TransactionRequest, String> {
    let to: zcash_address::ZcashAddress = to_address.parse().map_err(|e| format!("Bad address: {e}"))?;
    let value = Zatoshis::from_u64(amount_zatoshi).map_err(|_| "Bad amount")?;
    let memo_bytes = match memo_str {
        Some(m) => Some(MemoBytes::from(Memo::from_bytes(m.as_bytes()).map_err(|e| format!("Bad memo: {e}"))?)),
        None => None,
    };
    let payment = Payment::new(to, Some(value), memo_bytes, None, None, vec![]).map_err(|e| format!("Cannot create payment: {e:?}"))?;
    TransactionRequest::new(vec![payment]).map_err(|e| format!("{e:?}"))
}

fn send_proposal_lock_expiry(min_target: BlockHeight) -> BlockHeight {
    min_target + SEND_PROPOSAL_LOCK_BLOCKS
}

fn send_proposal_is_expired(min_target: BlockHeight, current: BlockHeight) -> bool {
    current > send_proposal_lock_expiry(min_target)
}

pub(super) async fn live_send_expiry_height(lwd_url: &str, min_target: BlockHeight) -> Result<BlockHeight, String> {
    let mut client = crate::wallet::sync_engine::open_lwd_channel(lwd_url).await.map_err(|e| format!("Connect: {e}"))?;
    let tip = crate::wallet::sync_engine::get_latest_block(&mut client).await.map_err(|e| format!("Get tip: {e}"))?;
    let tip_h = BlockHeight::from_u32(u32::try_from(tip.height).unwrap_or(0));
    Ok(std::cmp::max(tip_h, min_target) + SEND_PROPOSAL_LOCK_BLOCKS)
}

pub(super) fn proposal_input_refs(proposal: &zcash_client_backend::proposal::Proposal<WalletFeeRule, ReceivedNoteId>) -> Vec<zcash_client_backend::wallet::OutputRef> {
    proposal.steps().iter().flat_map(|step| {
        step.shielded_inputs().into_iter().flat_map(|inputs| {
            inputs.notes().iter().map(|note| zcash_client_backend::wallet::OutputRef::new(
                *note.txid(), PoolType::Shielded(note.note().pool()), u32::from(note.output_index()),
            ))
        }).chain(step.transparent_inputs().iter().map(|utxo| {
            let op = utxo.outpoint();
            zcash_client_backend::wallet::OutputRef::new(TxId::from_bytes(*op.hash()), PoolType::TRANSPARENT, op.n())
        }))
    }).collect()
}

fn ordinary_send_spend_pools() -> Vec<ShieldedPool> {
    vec![ShieldedPool::Sapling, ShieldedPool::Orchard, ShieldedPool::Ironwood]
}

fn ordinary_send_spend_policy() -> SpendPolicy {
    SpendPolicy::shielded_pools(ordinary_send_spend_pools())
}

// ======================== Send ========================

pub fn propose_send(
    db_path: &str, network: WalletNetwork, account_uuid: &str, send_flow_id: &str,
    to_address: &str, amount_zatoshi: u64, memo_str: Option<&str>,
) -> Result<ProposalResult, String> {
    if send_flow_id.is_empty() { return Err("Send flow id is required".into()); }
    with_wallet_db_write_lock("send.propose_send", || {
        let mut db = open_wallet_db(db_path, network)?;
        let account_id = parse_account_uuid(account_uuid)?;
        let request = build_send_request(to_address, amount_zatoshi, memo_str)?;
        let (change_strategy, input_selector) = zip317_helper();
        let spend_policy = ordinary_send_spend_policy();

        let proposal = wallet::propose_transfer::<_, _, _, _, shardtree::error::ShardTreeError<zcash_client_sqlite::wallet::commitment_tree::Error>>(
            &mut db, &network, account_id, &input_selector, &change_strategy,
            request, ConfirmationsPolicy::default(), &spend_policy,
            None, None,
        ).map_err(|e| format!("Propose transfer: {e}"))?;

        let needs_sapling = proposal.steps().iter().any(|s| s.involves(PoolType::Shielded(ShieldedPool::Sapling)));
        let fee: u64 = proposal.steps().iter().map(|s| u64::from(s.balance().fee_required())).sum();

        let lock_owner = LockOwner::random(&mut OsRng);
        let lock_expiry = send_proposal_lock_expiry(BlockHeight::from(proposal.min_target_height()));
        let input_refs = proposal_input_refs(&proposal);
        crate::wallet::sync::proposal_locks::persist(db_path, lock_owner, &input_refs, lock_expiry)?;
        if let Err(e) = db.lock_outputs(&input_refs, lock_owner, lock_expiry) {
            let _ = crate::wallet::sync::proposal_locks::remove(db_path, lock_owner);
            return Err(format!("Lock inputs: {e:?}"));
        }

        let mut store = PROPOSAL_STORE.lock().map_err(|e| format!("Lock store: {e}"))?;
        let id = store.next_id;
        store.next_id += 1;
        store.locks.insert(id, StoredProposalLock {
            proposal: proposal.clone(), network, db_path: db_path.to_string(),
            owner: lock_owner, send_flow_id: send_flow_id.to_string(),
        });
        store.proposals.insert(id, StoredProposal {
            proposal_id: id, proposal, proposed_tx_version: None,
            network, account_id, send_flow_id: send_flow_id.to_string(),
        });

        Ok(ProposalResult { proposal_id: id, needs_sapling_params: needs_sapling, fee_zatoshi: fee })
    })
}

pub fn estimate_fee(
    db_path: &str, network: WalletNetwork, account_uuid: &str,
    to_address: &str, amount_zatoshi: u64, memo_str: Option<&str>,
) -> Result<u64, String> {
    let mut db = open_wallet_db(db_path, network)?;
    let account_id = parse_account_uuid(account_uuid)?;
    let request = build_send_request(to_address, amount_zatoshi, memo_str)?;
    let (change_strategy, input_selector) = zip317_helper();
    let proposal = wallet::propose_transfer::<_, _, _, _, shardtree::error::ShardTreeError<zcash_client_sqlite::wallet::commitment_tree::Error>>(
        &mut db, &network, account_id, &input_selector, &change_strategy,
        request, ConfirmationsPolicy::default(), &ordinary_send_spend_policy(),
        None, None,
    ).map_err(|e| format!("Propose: {e}"))?;
    Ok(proposal.steps().iter().map(|s| u64::from(s.balance().fee_required())).sum())
}

pub(crate) fn estimate_send_max(
    db_path: &str, network: WalletNetwork, account_uuid: &str,
    to_address: &str, memo_str: Option<&str>,
) -> Result<SendMaxEstimateResult, String> {
    let mut db = open_wallet_db(db_path, network)?;
    let account_id = parse_account_uuid(account_uuid)?;
    let to: zcash_address::ZcashAddress = to_address.parse().map_err(|e| format!("Bad address: {e}"))?;
    let memo_bytes = match memo_str {
        Some(m) => Some(MemoBytes::from(Memo::from_bytes(m.as_bytes()).map_err(|e| format!("Memo: {e}"))?)),
        None => None,
    };
    let proposal = propose_send_max_transfer::<_, _, _, shardtree::error::ShardTreeError<zcash_client_sqlite::wallet::commitment_tree::Error>>(
        &mut db, &network, account_id,
        &[ShieldedPool::Sapling, ShieldedPool::Orchard, ShieldedPool::Ironwood],
        &StandardFeeRule::Zip317, to, memo_bytes,
        MaxSpendMode::MaxSpendable,
        ConfirmationsPolicy::default(), &LockedInputPolicy::Exclude, None,
    ).map_err(|e| format!("Send max: {e}"))?;
    let fee: u64 = proposal.steps().iter().map(|s| u64::from(s.balance().fee_required())).sum();
    let total_out: u64 = proposal.steps().iter().map(|s| {
        s.balance().proposed_change().iter().map(|c| u64::from(c.value())).sum::<u64>() + fee
    }).sum();
    Ok(SendMaxEstimateResult { amount_zatoshi: total_out - fee, fee_zatoshi: fee, needs_sapling_params: false })
}

// ======================== Execute ========================

pub async fn execute_proposal(
    db_path: &str, lightwalletd_url: &str, proposal_id: u64, send_flow_id: &str,
    seed: SecretVec<u8>, spend_params: Option<&str>, output_params: Option<&str>,
) -> Result<ExecuteProposalResult, String> {
    let stored = consume_stored_proposal(proposal_id, send_flow_id, "Proposal not found")?;
    execute_stored(db_path, lightwalletd_url, stored, seed, spend_params, output_params).await
}

pub async fn execute_proposal_with_seed_loader<F>(
    db_path: &str, lightwalletd_url: &str, proposal_id: u64, send_flow_id: &str,
    load_seed: F, spend_params: Option<&str>, output_params: Option<&str>,
) -> Result<ExecuteProposalResult, String>
where F: FnOnce(WalletNetwork, AccountUuid) -> Result<SecretVec<u8>, String> {
    let stored = consume_stored_proposal(proposal_id, send_flow_id, "Proposal not found")?;
    let seed = load_seed(stored.network, stored.account_id).map_err(|e| {
        let _ = finish_stored_proposal(proposal_id, send_flow_id, true); e
    })?;
    execute_stored(db_path, lightwalletd_url, stored, seed, spend_params, output_params).await
}

async fn execute_stored(
    db_path: &str, lightwalletd_url: &str, stored: StoredProposal,
    seed: SecretVec<u8>, _spend_params: Option<&str>, _output_params: Option<&str>,
) -> Result<ExecuteProposalResult, String> {
    let network = stored.network;
    let proposal_id = stored.proposal_id;
    let send_flow_id = stored.send_flow_id.clone();
    let proposal_lock = stored_proposal_lock(proposal_id, &send_flow_id)?;
    if proposal_lock.db_path != db_path {
        let _ = finish_stored_proposal(proposal_id, &send_flow_id, true);
        return Err("Wrong wallet DB".into());
    }
    let min_target = BlockHeight::from(stored.proposal.min_target_height());
    let live_expiry = live_send_expiry_height(lightwalletd_url, min_target).await.map_err(|e| {
        let _ = finish_stored_proposal(proposal_id, &send_flow_id, true); e
    })?;

    let create_result = with_wallet_db_write_lock("send.execute", || {
        let mut db = open_wallet_db(db_path, network)?;
        let (target_h, _) = db.get_target_and_anchor_heights(ConfirmationsPolicy::default().trusted())
            .map_err(|e| format!("Read heights: {e}"))?.ok_or("Wallet must sync")?;
        if send_proposal_is_expired(min_target, BlockHeight::from(target_h)) { return Err("Proposal expired".into()); }
        db.lock_outputs(&proposal_input_refs(&stored.proposal), proposal_lock.owner, live_expiry)
            .map_err(|e| format!("Relock: {e:?}"))?;
        crate::wallet::sync::proposal_locks::update_expiry(db_path, proposal_lock.owner, live_expiry)?;

        let account = db.get_account(stored.account_id).map_err(|e| format!("{e}"))?.ok_or("Account not found")?;
        let zip32_index = account.source().key_derivation().ok_or("No key derivation")?.account_index();
        let usk = UnifiedSpendingKey::from_seed(&network, seed.expose_secret(), zip32_index).map_err(|e| format!("USK: {e:?}"))?;
        drop(seed);

        let txids = create_proposed_transactions::<_, _, std::convert::Infallible, _, std::convert::Infallible, _>(
            &mut db, &network, &NoOpSpendProver, &NoOpOutputProver,
            &SpendingKeys::from_unified_spending_key(usk), OvkPolicy::Sender,
            &stored.proposal, Some(live_expiry),
        ).map_err(|e| format!("Create TX: {e}"))?;
        Ok::<_, String>(txids)
    });

    let txids = match create_result {
        Ok(txids) => { let _ = finish_stored_proposal(proposal_id, &send_flow_id, false); txids }
        Err(e) => { let _ = finish_stored_proposal(proposal_id, &send_flow_id, true); return Err(e); }
    };

    // Broadcast
    let txids: Vec<TxId> = txids.iter().cloned().collect();
    let read_conn = open_readonly_conn(db_path)?;
    let mut broadcast_ok = Vec::new();
    for txid in &txids {
        let raw_tx: Vec<u8> = read_conn.query_row("SELECT raw FROM transactions WHERE txid = ?1", rusqlite::params![txid.as_ref()], |r| r.get(0)).map_err(|e| format!("Get raw tx: {e}"))?;
        let mut client = crate::wallet::sync_engine::open_isolated_lwd_channel(lightwalletd_url).await.map_err(|e| format!("Connect: {e}"))?;
        let resp = crate::wallet::sync_engine::send_transaction(&mut client, &raw_tx).await.map_err(|e| format!("Broadcast: {e}"))?;
        if let Some(err) = crate::wallet::sync::broadcast::send_response_rejection_error(&resp) { return Err(err); }
        broadcast_ok.push(*txid);
        log::info!("send: broadcast {txid}");
    }
    let count = broadcast_ok.len() as u32;
    Ok(ExecuteProposalResult { txids: broadcast_ok, status: "broadcasted".to_string(), broadcasted_count: count, total_count: count, message: None })
}

// ======================== Shield ========================

pub(crate) fn get_shield_transparent_status(
    db_path: &str, network: WalletNetwork, account_uuid: &str,
) -> Result<ShieldTransparentStatus, String> {
    let db = open_wallet_db(db_path, network)?;
    let account_id = parse_account_uuid(account_uuid)?;
    let chain_height = db.chain_height().map_err(|e| format!("Chain height: {e}"))?.ok_or("Wallet must sync")?;
    #[cfg(feature = "transparent-inputs")]
    {
        let balances = db.get_transparent_balances(account_id, (chain_height + 1).into(), ConfirmationsPolicy::MIN)
            .map_err(|e| format!("Transparent balances: {e}"))?;
        let total: u64 = balances.values().map(|b| u64::from(*b)).sum();
        if total == 0 { return Ok(ShieldTransparentStatus { can_shield: false, fee_zatoshi: 0, shielded_zatoshi: 0, reason: "No transparent balance".into() }); }
        let fee = 10_000;
        Ok(ShieldTransparentStatus { can_shield: total > fee, fee_zatoshi: fee, shielded_zatoshi: total.saturating_sub(fee), reason: String::new() })
    }
    #[cfg(not(feature = "transparent-inputs"))]
    Ok(ShieldTransparentStatus { can_shield: false, fee_zatoshi: 0, shielded_zatoshi: 0, reason: "Transparent inputs not enabled".into() })
}

pub(crate) async fn shield_transparent_balance(
    _db_path: &str, _lightwalletd_url: &str, _network: WalletNetwork, _account_uuid: &str,
    _seed: secrecy::SecretVec<u8>,
) -> Result<ShieldTransparentResult, String> {
    Err("Shield not yet implemented in clean send.rs".into())
}

// ======================== Resubmit ========================

pub(crate) async fn resubmit_pending_transactions<ShouldExit>(
    db_path: &str, _lightwalletd_url: &str,
    client: &mut zcash_client_backend::proto::service::compact_tx_streamer_client::CompactTxStreamerClient<tonic::transport::Channel>,
    _current_height: u32, _excluded: &HashSet<Vec<u8>>, should_exit: ShouldExit,
) -> Result<ResubmitStats, String>
where ShouldExit: Fn() -> bool {
    let txs = crate::wallet::sync::transactions::get_resubmittable_txs(db_path, _current_height)?;
    let mut stats = ResubmitStats::default();
    for tx in txs {
        if should_exit() { break; }
        stats.attempted += 1;
        let resp = crate::wallet::sync_engine::send_transaction(client, &tx.raw_tx).await;
        match resp {
            Ok(resp) => {
                if let Some(err) = crate::wallet::sync::broadcast::send_response_rejection_error(&resp) {
                    log::warn!("resubmit: {} rejected: {err}", hex::encode(&tx.txid_bytes));
                    stats.failed += 1;
                } else {
                    stats.succeeded += 1;
                }
            }
            Err(e) => { log::warn!("resubmit: {} failed: {e}", hex::encode(&tx.txid_bytes)); stats.failed += 1; }
        }
    }
    Ok(stats)
}

// ======================== Public broadcast (for API layer) ========================

pub(crate) async fn broadcast_raw_transaction(
    client: &mut zcash_client_backend::proto::service::compact_tx_streamer_client::CompactTxStreamerClient<tonic::transport::Channel>,
    raw_tx: &[u8],
) -> Result<(), String> {
    let resp = crate::wallet::sync_engine::send_transaction(client, raw_tx).await.map_err(|e| format!("SendTransaction: {e}"))?;
    if let Some(err) = crate::wallet::sync::broadcast::send_response_rejection_error(&resp) { return Err(err); }
    Ok(())
}