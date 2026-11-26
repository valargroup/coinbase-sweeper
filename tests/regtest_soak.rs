//! Soak: both commands run as services while blocks are mined continuously (ignored by
//! default). SOAK_MINUTES sets the length (default 30).
//!   cargo test --release --test regtest_soak -- --ignored --nocapture

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use common::regtest::*;
use serde_json::{json, Value};
use zecd_regtest_harness::derive_funder_transparent_address;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "slow: 30 minutes by default"]
async fn regtest_soak_both_commands() {
    let Some(bin) = node_bin("regtest_soak_both_commands") else {
        return;
    };
    let minutes: u64 = std::env::var("SOAK_MINUTES")
        .ok()
        .and_then(|m| m.parse().ok())
        .unwrap_or(30);
    let pool = Pool::start(&bin).await;
    let start_height = pool.tip().await;
    pool.mine(130).await;
    let external = derive_funder_transparent_address(11).unwrap();
    let body = format!(
        "{}\n{}",
        sweep_section(&pool.taddr, 10),
        refill_section(&format!(
            "mode = \"drain_above\"\ndestination = \"{external}\"\n\
             external_destination = \"{external}\"\nreserve = 20\n\
             max_per_transfer = 100\nmax_per_day = 100000\n"
        ))
    );
    let cfg = pool.config("soak.toml", &pool.ua, "", &body);
    let cfg = cfg.to_str().unwrap();
    let sweep = pool.spawn_tool(&["sweep", "--config", cfg], "soak-sweep.log");
    let refill = pool.spawn_tool(&["refill", "--config", cfg], "soak-refill.log");
    let (sweep_pid, refill_pid) = (sweep.child.id(), refill.child.id());
    // Fail fast if either service refused to start.
    tokio::time::sleep(Duration::from_secs(10)).await;
    let mut sweep = sweep;
    let mut refill = refill;
    for (name, tool) in [("sweep", &mut sweep), ("refill", &mut refill)] {
        assert!(
            tool.child.try_wait().unwrap().is_none(),
            "{name} exited early: {}",
            tool.log()
        );
    }

    let deadline = Instant::now() + Duration::from_secs(minutes * 60);
    let mut mined = 0;
    while Instant::now() < deadline {
        pool.zebrad.generate_blocks(1).await.unwrap();
        mined += 1;
        tokio::time::sleep(Duration::from_secs(4)).await;
    }
    // Let both commands settle on a quiet chain: a few more blocks, then a full idle cycle.
    for _ in 0..6 {
        pool.mine(1).await;
        tokio::time::sleep(Duration::from_secs(8)).await;
    }
    let footprint = |pid| {
        (
            status_kb(pid, "VmHWM:"),
            status_kb(pid, "VmRSS:"),
            cpu_secs(pid),
        )
    };
    let (s_hwm, s_rss, s_cpu) = footprint(sweep_pid);
    let (r_hwm, r_rss, r_cpu) = footprint(refill_pid);
    drop(sweep);
    drop(refill);
    pool.mine(2).await;
    eprintln!(
        "SOAK {minutes} min, {mined} blocks mined. sweep: peak RSS {s_hwm} kB, RSS {s_rss} kB, \
         CPU {s_cpu:.1} s. refill: peak RSS {r_hwm} kB, RSS {r_rss} kB, CPU {r_cpu:.1} s"
    );

    // No errors in either log.
    for log in ["soak-sweep.log", "soak-refill.log"] {
        let text = std::fs::read_to_string(pool.dir.path().join(log)).unwrap();
        let errors: Vec<&str> = text.lines().filter(|l| l.contains(" ERROR ")).collect();
        assert!(errors.is_empty(), "{log}: {errors:#?}");
        eprintln!(
            "SOAK {log}: {} lines, {} cycles",
            text.lines().count(),
            text.matches("event=cycle_done").count()
        );
    }

    // The books, from the chain: every coinbase output paid to the pool, every transaction
    // spending one (a sweep), and every payment to the external destination (a refill).
    let tip = pool.tip().await;
    let mut coinbase: BTreeMap<(String, u64), u64> = BTreeMap::new();
    let mut spent: BTreeMap<(String, u64), String> = BTreeMap::new();
    let (mut swept_in, mut sweep_fees, mut refilled, mut refill_fees) = (0u64, 0i64, 0u64, 0i64);
    let mut sweeps = 0;
    let mut refills = 0;
    for h in 1..=tip {
        let hash = pool.zebrad.rpc("getblockhash", json!([h])).await.unwrap();
        let block = pool.zebrad.rpc("getblock", json!([hash, 1])).await.unwrap();
        for (i, txid) in block["tx"].as_array().unwrap().iter().enumerate() {
            let txid = txid.as_str().unwrap();
            let tx: Value = pool
                .zebrad
                .rpc("getrawtransaction", json!([txid, 1]))
                .await
                .unwrap();
            let vb = |pool_name: &str| tx[pool_name]["valueBalanceZat"].as_i64().unwrap_or(0);
            if i == 0 {
                for o in tx["vout"].as_array().unwrap() {
                    if o["scriptPubKey"]["addresses"][0] == json!(pool.taddr) {
                        coinbase.insert(
                            (txid.to_owned(), o["n"].as_u64().unwrap()),
                            o["valueZat"].as_u64().unwrap(),
                        );
                    }
                }
                continue;
            }
            let ins: Vec<(String, u64)> = inputs(&tx);
            if !ins.is_empty() && ins.iter().all(|o| coinbase.contains_key(o)) {
                sweeps += 1;
                let value: u64 = ins.iter().map(|o| coinbase[o]).sum();
                for o in ins {
                    assert!(
                        spent.insert(o.clone(), txid.to_owned()).is_none(),
                        "{o:?} twice"
                    );
                }
                swept_in += value;
                sweep_fees += value as i64 + vb("ironwood") + vb("orchard");
                eprintln!(
                    "SOAK sweep {txid} h={h} inputs_value={value} ironwood_vb={} orchard_vb={} \
                     vout={}",
                    vb("ironwood"),
                    vb("orchard"),
                    tx["vout"].as_array().unwrap().len()
                );
                assert!(tx["vout"].as_array().unwrap().is_empty(), "{tx}");
            }
            let to_ext: u64 = tx["vout"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|o| o["scriptPubKey"]["addresses"][0] == json!(external))
                .map(|o| o["valueZat"].as_u64().unwrap())
                .sum();
            if to_ext > 0 {
                refills += 1;
                assert!(tx["vin"].as_array().unwrap().is_empty(), "{tx}");
                refilled += to_ext;
                refill_fees += vb("ironwood") + vb("orchard") - to_ext as i64;
                eprintln!(
                    "SOAK refill {txid} h={h} paid={to_ext} ironwood_vb={} orchard_vb={} \
                     vout={} vin={}",
                    vb("ironwood"),
                    vb("orchard"),
                    tx["vout"].as_array().unwrap().len(),
                    tx["vin"].as_array().unwrap().len()
                );
            }
        }
    }
    // Includes rewards still immature, and the two seed blocks below the wallet's birthday.
    let unswept: Vec<_> = coinbase
        .keys()
        .filter(|o| !spent.contains_key(*o))
        .collect();
    for minconf in [0, 1] {
        eprintln!(
            "SOAK z_getbalance(ua, {minconf}) = {}",
            pool.call("z_getbalance", json!([pool.ua, minconf])).await
        );
    }
    let notes = pool.call("listunspent", json!([0, 9_999_999])).await;
    for n in notes.as_array().unwrap() {
        if n["pool"] != json!("transparent") {
            eprintln!("SOAK note {n}");
        }
    }
    let balance = pool.call("z_getbalance", json!([pool.ua, 1])).await;
    let balance = coinbase_sweeper::amount::Zat::from_json(&balance)
        .unwrap()
        .0 as i64;
    eprintln!(
        "SOAK books: {} coinbase outputs, {} swept in {sweeps} sweeps ({swept_in} zat, fees \
         {sweep_fees}), {refills} refills ({refilled} zat, fees {refill_fees}), {} unswept, \
         ironwood balance {balance} zat, start height {start_height}, tip {tip}",
        coinbase.len(),
        spent.len(),
        unswept.len()
    );
    let lhs = swept_in as i64 - sweep_fees - refill_fees;
    let rhs = balance + refilled as i64;
    // A refill can leave a change note no larger than the ZIP 317 marginal fee (5,000 zat):
    // spending it would cost what it holds, so wallets leave it out of the balance. At most
    // one such note per refill; anything else must balance exactly.
    let dust = lhs - rhs;
    eprintln!("SOAK books: swept - fees = {lhs}; ironwood balance + refilled = {rhs}; dust {dust}");
    assert!(
        (0..=5_000 * refills as i64).contains(&dust),
        "swept coinbase minus fees must equal Ironwood balance plus refilled (dust {dust})"
    );

    // Every reward that is mature at the end, and that the wallet can see, was swept once.
    let mature_now: BTreeSet<_> = pool.mature_coinbase().await;
    assert!(
        mature_now.len() < 10,
        "the loop leaves fewer than min_inputs mature rewards: {}",
        mature_now.len()
    );
}
