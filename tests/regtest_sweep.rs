//! `sweep` against a real Zakura node and zecd wallet. Skips unless ZECD_REGTEST_NODE=zakura,
//! ZAKURAD_BIN and ZECD_BIN are set.

mod common;

use std::collections::BTreeSet;
use std::time::Duration;

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
    let bad = pool.config("bad.toml", &sapling_ua, "", &sweep_section(&pool.taddr, 1));
    let (code, log) = pool
        .run_tool_mining(
            &["sweep", "--config", bad.to_str().unwrap(), "--once"],
            "bad.log",
        )
        .await;
    assert_eq!(code, 1, "{log}");
    assert!(log.contains("Sapling receiver"), "{log}");

    let cfg = pool.config("sweep.toml", &pool.ua, "", &sweep_section(&pool.taddr, 1));
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

#[tokio::test(flavor = "multi_thread")]
async fn regtest_sweep_killed_mid_batch_sweeps_everything_once() {
    let Some(bin) = node_bin("regtest_sweep_killed_mid_batch_sweeps_everything_once") else {
        return;
    };
    let pool = Pool::start(&bin).await;
    pool.mine(POOL_BLOCKS + 100).await;
    let from = pool.tip().await;
    let mature = pool.mature_coinbase().await;
    let cfg = pool.config("sweep.toml", &pool.ua, "", &sweep_section(&pool.taddr, 1));
    let cfg = cfg.to_str().unwrap();

    let mut tool = pool.spawn_tool(&["sweep", "--config", cfg], "killed.log");
    tool.wait_for_log("event=sweep_submitted", Duration::from_secs(120))
        .await;
    tool.kill();
    eprintln!("killed run:\n{}", tool.log());

    let (code, log) = pool
        .run_tool_mining(&["sweep", "--config", cfg, "--once"], "restart.log")
        .await;
    eprintln!("restarted run:\n{log}");
    assert_eq!(code, 0, "{log}");
    assert!(pool.mature_coinbase().await.is_empty(), "{log}");
    pool.mine(1).await;
    assert_swept_once(&pool, &mature, from).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn regtest_sweep_survives_wallet_restart_and_node_outage() {
    let Some(bin) = node_bin("regtest_sweep_survives_wallet_restart_and_node_outage") else {
        return;
    };
    let mut pool = Pool::start(&bin).await;
    pool.mine(POOL_BLOCKS + 100).await;
    let from = pool.tip().await;
    let mature = pool.mature_coinbase().await;
    let cfg = pool.config("sweep.toml", &pool.ua, "", &sweep_section(&pool.taddr, 5));
    let cfg = cfg.to_str().unwrap();
    let tool = pool.spawn_tool(&["sweep", "--config", cfg], "loop.log");

    // 1. Restart the wallet while the first sweep operation is running.
    tool.wait_for_log("event=sweep_submitted", Duration::from_secs(120))
        .await;
    pool.zecd.restart(&pool.zecd_cfg).await.unwrap();
    pool.sync().await;
    // Keep mining for confirmation waits until the backlog is gone and a cycle finds nothing.
    let mut served = 0;
    let before = tool.log().len();
    served = pool
        .drive(&tool, served, Duration::from_secs(600), |log| {
            log[before..].contains("event=nothing_due")
        })
        .await;
    pool.mine(1).await;
    assert_swept_once(&pool, &mature, from).await;

    // 2. Hang the node for several cycles: cycles are skipped, then the tool recovers.
    let before = tool.log().len();
    pool.zebrad.pause().unwrap();
    let paused_log = loop {
        let log = tool.log();
        if log[before..].matches("event=cycle_skipped").count() >= 3 {
            break log;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    pool.zebrad.resume().unwrap();
    assert!(
        paused_log[before..].contains("ERROR event=cycle_skipped"),
        "one error after repeated skips: {}",
        &paused_log[before..]
    );
    // New rewards mature after the outage and are swept.
    let from2 = pool.tip().await;
    pool.mine(5).await;
    let mature2 = pool.mature_coinbase().await;
    assert!(!mature2.is_empty());
    let before = tool.log().len();
    pool.drive(&tool, served, Duration::from_secs(600), |log| {
        log[before..].contains("event=recovered") && log[before..].contains("event=nothing_due")
    })
    .await;
    drop(tool);
    pool.mine(1).await;
    assert_swept_once(&pool, &mature2, from2).await;
    let log = std::fs::read_to_string(pool.dir.path().join("loop.log")).unwrap();
    let errors: Vec<&str> = log.lines().filter(|l| l.contains(" ERROR ")).collect();
    eprintln!("errors logged across both faults: {errors:#?}");
}
