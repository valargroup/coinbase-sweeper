//! `sweep` against a real Zakura node and zecd wallet. Skips unless ZECD_REGTEST_NODE=zakura,
//! ZAKURAD_BIN and ZECD_BIN are set.

mod common;

use std::collections::BTreeSet;

use common::regtest::*;
use serde_json::json;
use zecd_regtest_harness::derive_address_offline;

/// Pool blocks mined before maturity; with the blocks below them this leaves more than 50
/// mature rewards, so one cycle needs two batches.
const POOL_BLOCKS: u32 = 70;

#[tokio::test(flavor = "multi_thread")]
async fn regtest_sweep_shields_all_mature_coinbase_in_two_batches() {
    let Some(bin) = node_bin("regtest_sweep_shields_all_mature_coinbase_in_two_batches") else {
        return;
    };
    let pool = Pool::start(&bin).await;
    pool.mine(POOL_BLOCKS + 100).await;
    let mature = pool.mature_coinbase().await;
    assert!(mature.len() > 50, "only {} mature rewards", mature.len());

    // Startup refuses a unified address with a Sapling receiver.
    let sapling_ua = derive_address_offline(POOL_MNEMONIC, "sapling,orchard", 0).unwrap();
    let bad = pool.config("bad.toml", &sapling_ua, "", &sweep_section(&pool.taddr, ""));
    let (code, log) = pool
        .run_tool_mining(
            &["sweep", "--config", bad.to_str().unwrap(), "--once"],
            "bad.log",
        )
        .await;
    assert_eq!(code, 1, "{log}");
    assert!(log.contains("Sapling receiver"), "{log}");

    let cfg = pool.config("sweep.toml", &pool.ua, "", &sweep_section(&pool.taddr, ""));
    let started = std::time::Instant::now();
    let (code, log) = pool
        .run_tool_mining(
            &["sweep", "--config", cfg.to_str().unwrap(), "--once"],
            "sweep.log",
        )
        .await;
    eprintln!("sweep --once took {:?}\n{log}", started.elapsed());
    assert_eq!(code, 0, "{log}");

    // Nothing mature is left, even before the last sweep confirms.
    assert!(pool.mature_coinbase().await.is_empty(), "{log}");

    let txids = log_values(&log, "event=swept ", "txid");
    assert_eq!(txids.len(), 2, "two batches: {log}");
    let mut spent = Vec::new();
    for txid in &txids {
        let tx = pool.tx(txid).await;
        assert_eq!(tx["version"], json!(6), "{tx}");
        assert!(tx["vout"].as_array().unwrap().is_empty(), "{tx}");
        assert!(
            !tx["ironwood"]["actions"].as_array().unwrap().is_empty(),
            "{tx}"
        );
        assert!(tx["vShieldedOutput"].as_array().unwrap().is_empty(), "{tx}");
        spent.extend(inputs(&tx));
    }
    let spent_set: BTreeSet<_> = spent.iter().cloned().collect();
    assert_eq!(spent_set.len(), spent.len(), "an input was swept twice");
    assert!(
        mature.is_subset(&spent_set),
        "a mature reward was not swept"
    );

    pool.mine(1).await;
    let balance = pool.call("z_getbalance", json!([pool.ua, 1])).await;
    eprintln!("ironwood balance after sweep: {balance}; txids: {txids:?}");
    assert!(balance.as_f64().unwrap() > 0.0);
}
