//! Performance measurements on regtest (ignored by default; they take several minutes):
//!   cargo test --release --test regtest_perf -- --ignored --nocapture
//! The results go in the README's validation section.

mod common;

use std::time::Instant;

use common::regtest::*;
use serde_json::json;

/// Rewards to clear in the backlog run: at least 1,000.
const BACKLOG: u32 = 1_050;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "slow: run explicitly to measure"]
async fn regtest_perf_sweep_and_idle() {
    let Some(bin) = node_bin("regtest_perf_sweep_and_idle") else {
        return;
    };
    let pool = Pool::start(&bin).await;
    let wallet_pid = pid_of(&pool.zecd.datadir().display().to_string()).expect("zecd pid");

    let started = Instant::now();
    pool.mine(BACKLOG + 100).await;
    eprintln!(
        "PERF setup: mined {} blocks in {:?}",
        BACKLOG + 100,
        started.elapsed()
    );
    let mature = pool.mature_coinbase().await;
    assert!(mature.len() >= 1_000, "{}", mature.len());
    let from = pool.tip().await;

    // 1. One 50-input sweep: wall time from the call to the operation's success, and the
    //    wallet's CPU over the same span.
    let one = pool.config(
        "one.toml",
        &pool.ua,
        "",
        &sweep_section(&pool.taddr, 1).replace(
            "max_wait_blocks = 1000",
            "max_wait_blocks = 1000\nmax_batches_per_cycle = 1",
        ),
    );
    let cpu0 = cpu_secs(wallet_pid);
    let (code, log) = pool
        .run_tool_mining(
            &["sweep", "--config", one.to_str().unwrap(), "--once"],
            "one.log",
        )
        .await;
    let cpu1 = cpu_secs(wallet_pid);
    assert_eq!(code, 0, "{log}");
    let ms = |line: &str| -> i64 {
        // 2026-11-26T05:15:02.213Z -> milliseconds of the day.
        let t = &line[11..23];
        let p: Vec<i64> = t.split([':', '.']).map(|x| x.parse().unwrap()).collect();
        ((p[0] * 60 + p[1]) * 60 + p[2]) * 1000 + p[3]
    };
    let submitted = log
        .lines()
        .find(|l| l.contains("event=sweep_submitted"))
        .unwrap();
    let swept = log.lines().find(|l| l.contains("event=swept ")).unwrap();
    eprintln!(
        "PERF one 50-input sweep: {} ms from call to success; wallet CPU {:.2} s over the run",
        ms(swept) - ms(submitted),
        cpu1 - cpu0
    );

    // 2. The backlog, one block mined per batch (the tool waits for each to confirm).
    let backlog = pool.config(
        "backlog.toml",
        &pool.ua,
        "",
        &sweep_section(&pool.taddr, 1).replace(
            "max_wait_blocks = 1000",
            "max_wait_blocks = 1000\nmax_batches_per_cycle = 100",
        ),
    );
    let started = Instant::now();
    let cpu0 = cpu_secs(wallet_pid);
    let (code, log) = pool
        .run_tool_mining(
            &["sweep", "--config", backlog.to_str().unwrap(), "--once"],
            "backlog.log",
        )
        .await;
    let elapsed = started.elapsed();
    let cpu1 = cpu_secs(wallet_pid);
    assert_eq!(code, 0, "{log}");
    let inputs: u64 = log_values(&log, "event=swept ", "inputs")
        .iter()
        .filter_map(|v| v.parse::<u64>().ok())
        .sum();
    let batches = log_values(&log, "event=swept ", "txid").len();
    let per_hour = inputs as f64 / elapsed.as_secs_f64() * 3600.0;
    eprintln!(
        "PERF backlog: {inputs} outputs in {batches} batches in {:.1} s = {per_hour:.0} outputs/hour \
         (target >= 1440); wallet CPU {:.1} s",
        elapsed.as_secs_f64(),
        cpu1 - cpu0
    );
    assert!(pool.mature_coinbase().await.is_empty() || batches == 100);
    pool.mine(1).await;
    let swept_set = mature.clone();
    assert_swept_once(&pool, &swept_set, from).await;

    // 3. An idle cycle: nothing due.
    let idle = pool.config("idle.toml", &pool.ua, "", &sweep_section(&pool.taddr, 10));
    let started = Instant::now();
    let (code, log) = pool
        .run_tool_mining(
            &["sweep", "--config", idle.to_str().unwrap(), "--once"],
            "idle.log",
        )
        .await;
    assert_eq!(code, 0, "{log}");
    eprintln!(
        "PERF idle cycle: rpc_calls={:?} cycle_millis={:?}; whole process incl. startup {:?}",
        last_value(&log, "event=cycle_done", "rpc_calls"),
        last_value(&log, "event=cycle_done", "millis"),
        started.elapsed()
    );
    let balance = pool.call("z_getbalance", json!([pool.ua, 0])).await;
    eprintln!("PERF ironwood balance after backlog: {balance}");
}
