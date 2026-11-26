//! `refill` against a real Zakura node and zecd wallet. Skips unless ZECD_REGTEST_NODE=zakura,
//! ZAKURAD_BIN and ZECD_BIN are set.

mod common;

use std::time::Duration;

use common::regtest::*;
use serde_json::json;
use zecd_regtest_harness::derive_funder_transparent_address;

const ZAT: f64 = 100_000_000.0;

fn zat(zec: f64) -> u64 {
    (zec * ZAT).round() as u64
}

/// Asserts the refill transaction spends Ironwood only and pays `dest` exactly `amount`.
async fn assert_ironwood_only_payment(pool: &Pool, txid: &str, dest: &str, amount: u64) {
    let tx = pool.tx(txid).await;
    assert_eq!(tx["version"], json!(6), "{tx}");
    assert!(tx["vin"].as_array().unwrap().is_empty(), "{tx}");
    assert!(tx["vShieldedSpend"].as_array().unwrap().is_empty(), "{tx}");
    assert!(tx["vShieldedOutput"].as_array().unwrap().is_empty(), "{tx}");
    assert!(
        !tx["ironwood"]["actions"].as_array().unwrap().is_empty(),
        "{tx}"
    );
    let vout = tx["vout"].as_array().unwrap();
    assert_eq!(vout.len(), 1, "{tx}");
    assert_eq!(vout[0]["scriptPubKey"]["addresses"][0], json!(dest), "{tx}");
    assert_eq!(vout[0]["valueZat"].as_u64().unwrap(), amount, "{tx}");
}

#[tokio::test(flavor = "multi_thread")]
async fn regtest_refill_top_up_and_daily_cap() {
    let Some(bin) = node_bin("regtest_refill_top_up_and_daily_cap") else {
        return;
    };
    let pool = Pool::start(&bin).await;
    let funded = pool.fund_ironwood(70).await;
    eprintln!("ironwood spendable: {funded}");
    let dest = pool
        .call("getnewaddress", json!(["", "transparent"]))
        .await
        .as_str()
        .unwrap()
        .to_owned();
    let sink = derive_funder_transparent_address(7).unwrap();
    let cfg = pool.config(
        "refill.toml",
        &pool.ua,
        "",
        &refill_section(&format!(
            "mode = \"top_up\"\ndestination = \"{dest}\"\nlow_water = 10\ntarget = 25\n\
             max_per_transfer = 30\nmax_per_day = 50\n"
        )),
    );
    let args = ["refill", "--config", cfg.to_str().unwrap(), "--once"];

    // 1. Empty destination: brought up to its target.
    let (code, log) = pool.run_tool_mining(&args, "r1.log").await;
    eprintln!("{log}");
    assert_eq!(code, 0, "{log}");
    let txids = log_values(&log, "event=refilled", "txid");
    assert_eq!(txids.len(), 1, "{log}");
    assert_ironwood_only_payment(&pool, &txids[0], &dest, zat(25.0)).await;
    pool.mine(11).await;
    assert_eq!(zat(pool.t_balance(&dest).await), zat(25.0));

    // 2. The pool pays out from it (zecd sends the change to a fresh address), and 4 ZEC
    //    arrive back: the balance is below low_water but not empty, so the refill must send
    //    only the difference.
    pool.send_from(&dest, &sink, "20", "AllowFullyTransparent")
        .await;
    pool.send_from(&pool.ua, &dest, "4", "AllowRevealedRecipients")
        .await;
    pool.mine(11).await;
    let left = pool.t_balance(&dest).await;
    assert_eq!(zat(left), zat(4.0));
    let (code, log) = pool.run_tool_mining(&args, "r2.log").await;
    eprintln!("{log}");
    assert_eq!(code, 0, "{log}");
    let txids = log_values(&log, "event=refilled", "txid");
    assert_eq!(txids.len(), 1, "{log}");
    let difference = zat(25.0) - zat(left);
    assert_ironwood_only_payment(&pool, &txids[0], &dest, difference).await;
    pool.mine(11).await;
    assert_eq!(zat(pool.t_balance(&dest).await), zat(25.0));

    // 3. Another 24 ZEC out: topping up again would pass max_per_day (25 + 21 + 25 > 50).
    pool.send_from(&dest, &sink, "24", "AllowFullyTransparent")
        .await;
    let (code, log) = pool.run_tool_mining(&args, "r3.log").await;
    eprintln!("{log}");
    assert_eq!(code, 0, "{log}");
    assert!(log.contains("max_per_day"), "{log}");
    assert!(!log.contains("event=refill_submitting"), "{log}");
}

#[tokio::test(flavor = "multi_thread")]
async fn regtest_refill_external_drain_above() {
    let Some(bin) = node_bin("regtest_refill_external_drain_above") else {
        return;
    };
    let pool = Pool::start(&bin).await;
    let funded = pool.fund_ironwood(70).await;
    let external = derive_funder_transparent_address(9).unwrap();
    let body = format!(
        "mode = \"drain_above\"\ndestination = \"{external}\"\nreserve = 50\n\
         max_per_transfer = 1000\nmax_per_day = 1000\n"
    );

    // Without the double entry the tool refuses to start.
    let bad = pool.config("bad.toml", &pool.ua, "", &refill_section(&body));
    let (code, log) = pool
        .run_tool_mining(
            &["refill", "--config", bad.to_str().unwrap(), "--once"],
            "bad.log",
        )
        .await;
    assert_eq!(code, 1, "{log}");
    assert!(log.contains("does not belong to this wallet"), "{log}");

    let from = pool.tip().await;
    let good = pool.config(
        "good.toml",
        &pool.ua,
        "",
        &refill_section(&format!("{body}external_destination = \"{external}\"\n")),
    );
    let args = ["refill", "--config", good.to_str().unwrap(), "--once"];
    let (code, log) = pool.run_tool_mining(&args, "d1.log").await;
    eprintln!("{log}");
    assert_eq!(code, 0, "{log}");
    let txids = log_values(&log, "event=refilled", "txid");
    assert_eq!(txids.len(), 1, "{log}");
    let expected = zat(funded) - zat(50.0);
    assert_ironwood_only_payment(&pool, &txids[0], &external, expected).await;

    // The next run waits for that transfer to confirm and finds nothing above the reserve.
    let (code, log) = pool.run_tool_mining(&args, "d2.log").await;
    eprintln!("{log}");
    assert_eq!(code, 0, "{log}");
    assert!(log.contains("event=wait_confirmation cmd=refill"), "{log}");
    assert!(log.contains("not above reserve"), "{log}");
    let paid = pool.payments_to(&external, from).await;
    assert_eq!(
        paid,
        vec![(txids[0].clone(), expected)],
        "paid once, confirmed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn regtest_refill_killed_mid_send_never_sends_twice() {
    let Some(bin) = node_bin("regtest_refill_killed_mid_send_never_sends_twice") else {
        return;
    };
    let pool = Pool::start(&bin).await;
    pool.fund_ironwood(70).await;
    let dest = pool
        .call("getnewaddress", json!(["", "transparent"]))
        .await
        .as_str()
        .unwrap()
        .to_owned();
    let cfg = pool.config(
        "refill.toml",
        &pool.ua,
        "",
        &refill_section(&format!(
            "mode = \"top_up\"\ndestination = \"{dest}\"\nlow_water = 10\ntarget = 25\n\
             max_per_transfer = 30\nmax_per_day = 100\n"
        )),
    );
    let cfg = cfg.to_str().unwrap();
    let from = pool.tip().await;

    // Kill the tool the moment it has recorded the transfer, before it sees the result.
    let mut tool = pool.spawn_tool(&["refill", "--config", cfg], "killed.log");
    tool.wait_for_log("event=refill_submitting", Duration::from_secs(120))
        .await;
    tool.kill();
    let state = std::fs::read_to_string(pool.state_dir().join("refill.state")).unwrap();
    eprintln!("killed run:\n{}\nstate: {state}", tool.log());
    assert!(state.contains("\"in_flight\": {"), "{state}");

    // Restart twice: the interrupted transfer is settled first, and only ever sent once.
    let (code1, log1) = pool
        .run_tool_mining(&["refill", "--config", cfg, "--once"], "restart1.log")
        .await;
    eprintln!("restart 1 (exit {code1}):\n{log1}");
    pool.mine(11).await;
    let (code2, log2) = pool
        .run_tool_mining(&["refill", "--config", cfg, "--once"], "restart2.log")
        .await;
    eprintln!("restart 2 (exit {code2}):\n{log2}");
    pool.mine(2).await;
    let paid = pool.payments_to(&dest, from).await;
    eprintln!("payments to the destination: {paid:?}");
    if log1.contains("event=refill_unsettled") || log1.contains("event=refill_settling") {
        // Could not be settled: nothing more may be sent until an operator resolves it.
        assert_eq!(code1, 2, "{log1}");
        assert!(paid.len() <= 1, "{paid:?}");
        assert!(!log2.contains("event=refill_submitted"), "{log2}");
    } else {
        assert_eq!(paid.len(), 1, "exactly one transfer: {paid:?}");
        assert_eq!(paid[0].1, 25 * 100_000_000);
    }
}
