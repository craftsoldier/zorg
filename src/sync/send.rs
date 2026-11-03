//! Software-wallet send, shield, and resubmit flows.
//!
//! Uses `zcash_client_backend`'s `propose_transfer` + `create_proposed_transactions`
//! pipeline.

use std::collections::HashSet;
use std::num::NonZeroUsize;

use rand::{rand_core::UnwrapErr, rngs::SysRng};
use secrecy::{ExposeSecret, SecretVec};

use zcash_client_backend::data_api::wallet::input_selection::{GreedyInputSelector, SpendPolicy};
use zcash_client_backend::{
    data_api::{
        wallet::{self, create_proposed_transactions, ConfirmationsPolicy, SpendingKeys},
        Account as _, OutputLockStore, WalletRead,
    },
    fees::{zip317::MultiOutputChangeStrategy, DustOutputPolicy, SplitPolicy, StandardFeeRule},
    wallet::{LockOwner, OvkPolicy},
    zip321::{Payment, TransactionRequest},
};
use zcash_client_sqlite::{util::SystemClock, AccountUuid, ReceivedNoteId};
use zcash_keys::keys::UnifiedSpendingKey;
use zcash_primitives::transaction::TxId;
use zcash_protocol::{
    consensus::BlockHeight,
    memo::{Memo, MemoBytes},
    value::Zatoshis,
    PoolType, ShieldedPool,
};

use crate::account::parse_account_uuid;
use crate::db::{with_wallet_db_write_lock, WalletDatabase};
use crate::network::WalletNetwork;
use crate::sync::{open_readonly_conn, open_wallet_db, proposal_locks};

pub type WalletFeeRule = StandardFeeRule;

const SEND_PROPOSAL_LOCK_BLOCKS: u32 = 40;

// ======================== Result Structs ========================

/// A send proposal whose inputs are locked to `owner` until it is executed or released.
pub struct ProposalResult {
    proposal: zcash_client_backend::proposal::Proposal<WalletFeeRule, ReceivedNoteId>,
    network: WalletNetwork,
    account_id: AccountUuid,
    db_path: String,
    owner: LockOwner,
    pub fee_zatoshi: u64,
}

pub struct ExecuteProposalResult {
    pub txids: Vec<TxId>,
    pub status: String,
    pub broadcasted_count: u32,
    pub total_count: u32,
    pub message: Option<String>,
}

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ResubmitStats {
    pub attempted: usize,
    pub succeeded: usize,
    pub failed: usize,
}

/// The file at `path`, opened and rewound, if its BLAKE2b-512 hash is `expected`.
fn open_verified(path: &std::path::Path, expected: &str) -> Option<std::fs::File> {
    use blake2b_simd::Params;
    use std::io::{Read, Seek};

    let mut file = std::fs::File::open(path).ok()?;
    let mut state = Params::new().hash_length(64).to_state();
    let mut buffer = [0; 8192];
    loop {
        let n = file.read(&mut buffer).ok()?;
        if n == 0 {
            break;
        }
        state.update(&buffer[..n]);
    }
    if hex::encode(state.finalize().as_bytes()) != expected {
        return None;
    }
    file.rewind().ok()?;
    Some(file)
}

/// The Sapling parameter file `filename`, opened from ZcashParams if its hash
/// matches, otherwise from zorg.app/Contents/Resources.
fn open_param(home: &str, filename: &str, expected_hash: &str) -> Result<std::fs::File, String> {
    let user = std::path::Path::new(home)
        .join("Library/Application Support/ZcashParams")
        .join(filename);
    if let Some(file) = open_verified(&user, expected_hash) {
        return Ok(file);
    }

    // Otherwise, use the copy bundled in zorg.app/Contents/Resources
    let exe = std::env::current_exe()
        .and_then(|path| path.canonicalize())
        .map_err(|e| format!("Failed to locate the zorg executable: {e}"))?;
    if let Some(contents) = exe.parent().and_then(|macos| macos.parent()) {
        if let Some(file) = open_verified(&contents.join("Resources").join(filename), expected_hash)
        {
            return Ok(file);
        }
    }

    Err(format!(
        "Sapling parameter {} not found in ZcashParams or the zorg app, or hash was invalid.",
        filename
    ))
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

fn build_send_request(
    to_address: &str,
    amount_zatoshi: u64,
    memo_str: Option<&str>,
) -> Result<TransactionRequest, String> {
    let to: zcash_address::ZcashAddress = to_address
        .parse()
        .map_err(|e| format!("Bad address: {e}"))?;
    let value = Zatoshis::from_u64(amount_zatoshi).map_err(|_| "Bad amount")?;
    let memo_bytes = match memo_str {
        Some(m) => Some(MemoBytes::from(
            Memo::from_bytes(m.as_bytes()).map_err(|e| format!("Bad memo: {e}"))?,
        )),
        None => None,
    };
    let payment = Payment::new(to, Some(value), memo_bytes, None, None, vec![])
        .map_err(|e| format!("Cannot create payment: {e:?}"))?;
    TransactionRequest::new(vec![payment]).map_err(|e| format!("{e:?}"))
}

fn send_proposal_lock_expiry(min_target: BlockHeight) -> BlockHeight {
    min_target + SEND_PROPOSAL_LOCK_BLOCKS
}

fn send_proposal_is_expired(min_target: BlockHeight, current: BlockHeight) -> bool {
    current > send_proposal_lock_expiry(min_target)
}

pub(super) async fn live_send_expiry_height(
    lwd_url: &str,
    min_target: BlockHeight,
) -> Result<BlockHeight, String> {
    let mut client = crate::sync_engine::open_lwd_channel(lwd_url)
        .await
        .map_err(|e| format!("Connect: {e}"))?;
    let tip = crate::sync_engine::get_latest_block(&mut client)
        .await
        .map_err(|e| format!("Get tip: {e}"))?;
    let tip_h = BlockHeight::from_u32(u32::try_from(tip.height).unwrap_or(0));
    Ok(std::cmp::max(tip_h, min_target) + SEND_PROPOSAL_LOCK_BLOCKS)
}

pub(super) fn proposal_input_refs(
    proposal: &zcash_client_backend::proposal::Proposal<WalletFeeRule, ReceivedNoteId>,
) -> Vec<zcash_client_backend::wallet::OutputRef> {
    proposal
        .steps()
        .iter()
        .flat_map(|step| {
            step.shielded_inputs()
                .into_iter()
                .flat_map(|inputs| {
                    inputs.notes().iter().map(|note| {
                        zcash_client_backend::wallet::OutputRef::new(
                            *note.txid(),
                            PoolType::Shielded(note.note().pool()),
                            u32::from(note.output_index()),
                        )
                    })
                })
                .chain(step.transparent_inputs().iter().map(|utxo| {
                    let op = utxo.outpoint();
                    zcash_client_backend::wallet::OutputRef::new(
                        TxId::from_bytes(*op.hash()),
                        PoolType::TRANSPARENT,
                        op.n(),
                    )
                }))
        })
        .collect()
}

fn ordinary_send_spend_pools() -> Vec<ShieldedPool> {
    vec![
        ShieldedPool::Sapling,
        ShieldedPool::Orchard,
        ShieldedPool::Ironwood,
    ]
}

fn ordinary_send_spend_policy() -> SpendPolicy {
    SpendPolicy::shielded_pools(ordinary_send_spend_pools())
}

// ======================== Send ========================

pub fn propose_send(
    db_path: &str,
    network: WalletNetwork,
    account_uuid: &str,
    to_address: &str,
    amount_zatoshi: u64,
    memo_str: Option<&str>,
) -> Result<ProposalResult, String> {
    with_wallet_db_write_lock("send.propose_send", || {
        let mut db = open_wallet_db(db_path, network)?;
        let account_id = parse_account_uuid(account_uuid)?;
        let request = build_send_request(to_address, amount_zatoshi, memo_str)?;
        let (change_strategy, input_selector) = zip317_helper();
        let spend_policy = ordinary_send_spend_policy();

        let proposal = wallet::propose_transfer::<
            _,
            _,
            _,
            _,
            shardtree::error::ShardTreeError<zcash_client_sqlite::wallet::commitment_tree::Error>,
        >(
            &mut db,
            &network,
            account_id,
            &input_selector,
            &change_strategy,
            request,
            ConfirmationsPolicy::default(),
            &spend_policy,
            None,
            None,
        )
        .map_err(|e| format!("Propose transfer: {e}"))?;

        let fee: u64 = proposal
            .steps()
            .iter()
            .map(|s| u64::from(s.balance().fee_required()))
            .sum();

        let lock_owner = LockOwner::random(&mut UnwrapErr(SysRng));
        let lock_expiry =
            send_proposal_lock_expiry(BlockHeight::from(proposal.min_target_height()));
        let input_refs = proposal_input_refs(&proposal);
        proposal_locks::persist(db_path, lock_owner, &input_refs, lock_expiry)?;
        if let Err(e) = db.lock_outputs(&input_refs, lock_owner, lock_expiry) {
            let _ = proposal_locks::remove(db_path, lock_owner);
            return Err(format!("Lock inputs: {e:?}"));
        }

        Ok(ProposalResult {
            proposal,
            network,
            account_id,
            db_path: db_path.to_string(),
            owner: lock_owner,
            fee_zatoshi: fee,
        })
    })
}

pub fn estimate_fee(
    db_path: &str,
    network: WalletNetwork,
    account_uuid: &str,
    to_address: &str,
    amount_zatoshi: u64,
    memo_str: Option<&str>,
) -> Result<u64, String> {
    let mut db = open_wallet_db(db_path, network)?;
    let account_id = parse_account_uuid(account_uuid)?;
    let request = build_send_request(to_address, amount_zatoshi, memo_str)?;
    let (change_strategy, input_selector) = zip317_helper();
    let proposal = wallet::propose_transfer::<
        _,
        _,
        _,
        _,
        shardtree::error::ShardTreeError<zcash_client_sqlite::wallet::commitment_tree::Error>,
    >(
        &mut db,
        &network,
        account_id,
        &input_selector,
        &change_strategy,
        request,
        ConfirmationsPolicy::default(),
        &ordinary_send_spend_policy(),
        None,
        None,
    )
    .map_err(|e| format!("Propose: {e}"))?;
    Ok(proposal
        .steps()
        .iter()
        .map(|s| u64::from(s.balance().fee_required()))
        .sum())
}

// ======================== Execute ========================

/// Forgets `stored`'s lock record, first unlocking its inputs when `release_inputs`.
fn finish_proposal(stored: &ProposalResult, release_inputs: bool) -> Result<(), String> {
    if !release_inputs {
        return proposal_locks::remove(&stored.db_path, stored.owner);
    }
    with_wallet_db_write_lock("sync.unlock_proposal", || {
        let mut db = open_wallet_db(&stored.db_path, stored.network)?;
        wallet::unlock_proposal_inputs(&mut db, &stored.proposal, stored.owner)
            .map_err(|e| format!("Unlock abandoned send proposal inputs: {e}"))?;
        proposal_locks::remove(&stored.db_path, stored.owner)
    })
}

pub async fn execute_proposal_with_seed_loader<F>(
    lightwalletd_url: &str,
    stored: ProposalResult,
    load_seed: F,
    on_broadcast: &dyn Fn(),
) -> Result<ExecuteProposalResult, String>
where
    F: FnOnce(WalletNetwork, AccountUuid) -> Result<SecretVec<u8>, String>,
{
    let seed = load_seed(stored.network, stored.account_id).inspect_err(|_| {
        let _ = finish_proposal(&stored, true);
    })?;
    execute_stored(lightwalletd_url, stored, seed, on_broadcast).await
}

async fn execute_stored(
    lightwalletd_url: &str,
    stored: ProposalResult,
    seed: SecretVec<u8>,
    on_broadcast: &dyn Fn(),
) -> Result<ExecuteProposalResult, String> {
    let network = stored.network;
    let db_path = stored.db_path.as_str();
    let min_target = BlockHeight::from(stored.proposal.min_target_height());
    let live_expiry = live_send_expiry_height(lightwalletd_url, min_target)
        .await
        .inspect_err(|_| {
            let _ = finish_proposal(&stored, true);
        })?;

    let create_result = with_wallet_db_write_lock("send.execute", || {
        let mut db = open_wallet_db(db_path, network)?;
        let (target_h, _) = db
            .get_target_and_anchor_heights(ConfirmationsPolicy::default().trusted())
            .map_err(|e| format!("Read heights: {e}"))?
            .ok_or("Wallet must sync")?;
        if send_proposal_is_expired(min_target, BlockHeight::from(target_h)) {
            return Err("Proposal expired".into());
        }
        db.lock_outputs(
            &proposal_input_refs(&stored.proposal),
            stored.owner,
            live_expiry,
        )
        .map_err(|e| format!("Relock: {e:?}"))?;
        proposal_locks::update_expiry(db_path, stored.owner, live_expiry)?;

        let account = db
            .get_account(stored.account_id)
            .map_err(|e| format!("{e}"))?
            .ok_or("Account not found")?;
        let zip32_index = account
            .source()
            .key_derivation()
            .ok_or("No key derivation")?
            .account_index();
        let usk = UnifiedSpendingKey::from_seed(&network, seed.expose_secret(), zip32_index)
            .map_err(|e| format!("USK: {e:?}"))?;
        drop(seed);

        let home = std::env::var("HOME").map_err(|_| "HOME environment variable not set")?;
        let spend_file = open_param(
            &home,
            "sapling-spend.params",
            "8270785a1a0d0bc77196f000ee6d221c9c9894f55307bd9357c3f0105d31ca63991ab91324160d8f53e2bbd3c2633a6eb8bdf5205d822e7f3f73edac51b2b70c",
        )?;
        let output_file = open_param(
            &home,
            "sapling-output.params",
            "657e3d38dbb5cb5e7dd2970e8b03d69b4787dd907285b5a7f0790dcc8072f60bf593b32cc2d1c030e00ff5ae64bf84c5c3beb84ddc841d48264b4a171744d028",
        )?;

        let spend_prover = sapling_crypto::circuit::SpendParameters::read(
            std::io::BufReader::new(spend_file),
            false,
        )
        .map_err(|e| format!("Failed to read sapling-spend.params: {}", e))?;
        let output_prover = sapling_crypto::circuit::OutputParameters::read(
            std::io::BufReader::new(output_file),
            false,
        )
        .map_err(|e| format!("Failed to read sapling-output.params: {}", e))?;

        let txids = create_proposed_transactions::<
            _,
            _,
            std::convert::Infallible,
            _,
            std::convert::Infallible,
            _,
        >(
            &mut db,
            &network,
            &SystemClock,
            &mut UnwrapErr(SysRng),
            &spend_prover,
            &output_prover,
            &SpendingKeys::from_unified_spending_key(usk),
            OvkPolicy::Sender,
            &stored.proposal,
            Some(live_expiry),
        )
        .map_err(|e| format!("Create TX: {e}"))?;
        Ok::<_, String>(txids)
    });

    let txids = match create_result {
        Ok(txids) => {
            let _ = finish_proposal(&stored, false);
            txids
        }
        Err(e) => {
            let _ = finish_proposal(&stored, true);
            return Err(e);
        }
    };

    // Broadcast
    on_broadcast();
    let txids: Vec<TxId> = txids.iter().cloned().collect();
    let read_conn = open_readonly_conn(db_path)?;
    let mut broadcast_ok = Vec::new();
    for txid in &txids {
        let raw_tx: Vec<u8> = read_conn
            .query_row(
                "SELECT raw FROM transactions WHERE txid = ?1",
                rusqlite::params![txid.as_ref()],
                |r| r.get(0),
            )
            .map_err(|e| format!("Get raw tx: {e}"))?;
        let mut client = crate::sync_engine::open_isolated_lwd_channel(lightwalletd_url)
            .await
            .map_err(|e| format!("Connect: {e}"))?;
        let resp = crate::sync_engine::send_transaction(&mut client, &raw_tx)
            .await
            .map_err(|e| format!("Broadcast: {e}"))?;
        if let Some(err) = crate::sync::broadcast::send_response_rejection_error(&resp) {
            return Err(err);
        }
        broadcast_ok.push(*txid);
        log::info!("send: broadcast {txid}");
    }
    let count = broadcast_ok.len() as u32;
    Ok(ExecuteProposalResult {
        txids: broadcast_ok,
        status: "broadcasted".to_string(),
        broadcasted_count: count,
        total_count: count,
        message: None,
    })
}

// ======================== Shield ========================

// ======================== Resubmit ========================

pub(crate) async fn resubmit_pending_transactions<ShouldExit>(
    db_path: &str,
    client: &mut zcash_client_backend::proto::service::compact_tx_streamer_client::CompactTxStreamerClient<tonic::transport::Channel>,
    current_height: u32,
    excluded: &HashSet<Vec<u8>>,
    should_exit: ShouldExit,
) -> Result<ResubmitStats, String>
where
    ShouldExit: Fn() -> bool,
{
    let txs = crate::sync::transactions::get_resubmittable_txs_excluding(
        db_path,
        current_height,
        excluded,
    )?;
    let mut stats = ResubmitStats::default();
    for tx in txs {
        if should_exit() {
            break;
        }
        stats.attempted += 1;
        let resp = crate::sync_engine::send_transaction(client, &tx.raw_tx).await;
        match resp {
            Ok(resp) => {
                if let Some(err) = crate::sync::broadcast::send_response_rejection_error(&resp) {
                    log::warn!("resubmit: {} rejected: {err}", tx.txid);
                    stats.failed += 1;
                } else {
                    stats.succeeded += 1;
                }
            }
            Err(e) => {
                log::warn!("resubmit: {} failed: {e}", tx.txid);
                stats.failed += 1;
            }
        }
    }
    Ok(stats)
}

// ======================== Public broadcast (for API layer) ========================
