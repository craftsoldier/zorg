mod common;

use common::{
    create_wallet, ensure_regtest_up, exclusive_regtest, fund_wallet, get_balance,
    get_transaction_history, sync_wallet,
};

#[test]
#[ignore = "requires Dockerized zcashd/lightwalletd regtest services"]
fn create_wallet_receives_funds_and_syncs_balance() {
    let _guard = exclusive_regtest();
    ensure_regtest_up();

    let (tempdir, wallet) = create_wallet("Regtest Account");
    let db_path = tempdir.path().join("zcash_wallet.db");
    println!("wallet ua={}", wallet.unified_address);

    let txid = fund_wallet(&wallet.unified_address, "1.0");
    assert!(!txid.is_empty(), "funding should return a txid");
    println!("funding txid={txid}");

    sync_wallet(&db_path);

    let balance = get_balance(&db_path, &wallet.account_uuid);
    println!(
        "post-sync balance spendable={} total={} orchard={} sapling={} transparent={}",
        balance.spendable, balance.total, balance.orchard, balance.sapling, balance.transparent
    );
    assert!(
        balance.spendable >= 100_000_000,
        "expected at least 1 ZEC after funding, got spendable={} total={}",
        balance.spendable,
        balance.total
    );

    let history = get_transaction_history(&db_path, &wallet.account_uuid);
    assert!(
        history.iter().any(|tx| tx.account_balance_delta > 0),
        "expected a positive receive in transaction history"
    );
}
