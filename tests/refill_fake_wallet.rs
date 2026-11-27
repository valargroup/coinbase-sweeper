//! `refill` against a scripted wallet: sending, caps, and settling an interrupted transfer.

mod common;

use std::time::Duration;

use common::fake_rpc::*;
use serde_json::{json, Value};

fn run(cfg: &std::path::Path, extra: &[&str]) -> (i32, String) {
    let mut args = vec!["refill", "--config", cfg.to_str().unwrap()];
    args.extend_from_slice(extra);
    run_bin(&args)
}

fn state(dir: &std::path::Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(dir.join("state/refill.state")).unwrap()).unwrap()
}

#[test]
fn top_up_sends_the_difference_and_records_it() {
    let f = healthy_refill_wallet("100", "4");
    let dir = tempfile::tempdir().unwrap();
    let cfg = refill_config(dir.path(), &f.url);
    let (code, err) = run(&cfg, &["--once"]);
    assert_eq!(code, 0, "{err}");
    let p = f.params("z_sendmany");
    assert_eq!(p.len(), 1, "{err}");
    assert_eq!(p[0][0], json!(UA));
    assert_eq!(p[0][1][0]["address"], json!(DEST));
    assert_eq!(p[0][1][0]["amount"].to_string(), "21.00000000");
    assert_eq!(p[0][2], json!(10), "minconf");
    assert_eq!(p[0][3], Value::Null, "the tool never sets a fee");
    assert_eq!(p[0][4], json!("AllowRevealedRecipients"));
    let s = state(dir.path());
    assert_eq!(s["sent_today"], json!(2_100_000_000u64));
    assert_eq!(s["in_flight"], Value::Null);
    assert_eq!(s["unconfirmed"], json!("refilltx"));
}

#[test]
fn above_low_water_sends_nothing() {
    let f = healthy_refill_wallet("100", "12");
    let dir = tempfile::tempdir().unwrap();
    let (code, err) = run(&refill_config(dir.path(), &f.url), &["--once"]);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("not below low_water"), "{err}");
    assert_eq!(f.calls("z_sendmany"), 0);
}

#[test]
fn minus_six_is_logged_and_not_retried() {
    let f = healthy_refill_wallet("100", "4");
    f.on("z_sendmany", |_| rpc_err(-6, "Insufficient funds"));
    let dir = tempfile::tempdir().unwrap();
    let (code, err) = run(&refill_config(dir.path(), &f.url), &["--once"]);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("insufficient_funds"), "{err}");
    assert_eq!(f.calls("z_sendmany"), 1);
    assert_eq!(state(dir.path())["sent_today"], json!(0), "refunded");
}

#[test]
fn timed_out_send_is_not_resent_and_stays_unsettled() {
    let f = healthy_refill_wallet("100", "4");
    f.on("z_sendmany", |_| {
        Reply::Hang(
            Duration::from_secs(4),
            Box::new(Reply::Ok(json!("opid-late"))),
        )
    });
    // The wallet never lists an operation for it, and its history shows nothing.
    let dir = tempfile::tempdir().unwrap();
    let cfg = refill_config(dir.path(), &f.url);
    let (code, err) = run(&cfg, &["--once"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("event=submit_unknown"), "{err}");
    assert!(err.contains("event=refill_settling"), "{err}");
    let (code, err) = run(&cfg, &["--once"]);
    assert_eq!(code, 2, "{err}");
    assert_eq!(f.calls("z_sendmany"), 1, "never resent: {err}");
    assert!(state(dir.path())["in_flight"].is_object());
}

#[test]
fn interrupted_refill_settled_by_its_operation() {
    let f = healthy_refill_wallet("100", "24");
    f.on("z_listoperationids", |_| Reply::Ok(json!(["opid-old"])));
    let dir = tempfile::tempdir().unwrap();
    interrupted_state(dir.path(), Some("opid-old"), true, 30);
    f.on("getrawtransaction", |_| {
        Reply::Ok(v6_refill_tx(DEST, 2_000_000_000))
    });
    let (code, err) = run(&refill_config(dir.path(), &f.url), &["--once"]);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("outcome=ok_recovered"), "{err}");
    assert_eq!(f.calls("z_sendmany"), 0, "{err}");
    let s = state(dir.path());
    assert_eq!(s["in_flight"], Value::Null);
    assert_eq!(s["sent_today"], json!(2_000_000_000u64), "counted once");
}

#[test]
fn interrupted_refill_settled_by_wallet_history() {
    let f = healthy_refill_wallet("100", "24");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    f.on("listtransactions", move |_| {
        Reply::Ok(
            json!([{"address": DEST, "category": "receive", "amount": 20,
                          "txid": "histtx", "time": now - 10}]),
        )
    });
    f.on("getrawtransaction", |_| {
        Reply::Ok(v6_refill_tx(DEST, 2_000_000_000))
    });
    let dir = tempfile::tempdir().unwrap();
    interrupted_state(dir.path(), None, true, 30);
    let (code, err) = run(&refill_config(dir.path(), &f.url), &["--once"]);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("txid=histtx"), "{err}");
    assert_eq!(f.calls("z_sendmany"), 0);
}

#[test]
fn interrupted_refill_that_cannot_be_settled_sends_nothing() {
    // The note the source held before the send is gone, but nothing shows where it went.
    let f = healthy_refill_wallet("100", "4");
    f.on("z_listunspent", |_| Reply::Ok(json!([])));
    let dir = tempfile::tempdir().unwrap();
    interrupted_state(dir.path(), None, true, 1000);
    let cfg = refill_config(dir.path(), &f.url);
    let (code, err) = run(&cfg, &["--once"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("event=refill_unsettled"), "{err}");
    assert_eq!(f.calls("z_sendmany"), 0);
    // --check shows it, and the operator's verdict clears it.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_coinbase-sweeper"))
        .args(["refill", "--config", cfg.to_str().unwrap(), "--check"])
        .env("SWEEPER_RPC_PASSWORD", "pw")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("INTERRUPTED refill"));
    let (code, err) = run(&cfg, &["--resolve-interrupted", "not-sent"]);
    assert_eq!(code, 0, "{err}");
    let s = state(dir.path());
    assert_eq!(s["in_flight"], Value::Null);
    assert_eq!(s["sent_today"], json!(0));
    let (code, err) = run(&cfg, &["--once"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(f.calls("z_sendmany"), 1);
}

#[test]
fn interrupted_refill_provably_not_sent_is_refunded_then_sent_once() {
    // Ten minutes on, no operation, no history entry, and the note the wallet held before the
    // send is still unspent: no funds moved. The refund happens now; a new send only on the
    // next cycle.
    let f = healthy_refill_wallet("100", "4");
    let dir = tempfile::tempdir().unwrap();
    interrupted_state(dir.path(), None, true, 1000);
    let cfg = refill_config(dir.path(), &f.url);
    let (code, err) = run(&cfg, &["--once"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("event=refill_not_sent"), "{err}");
    assert_eq!(f.calls("z_sendmany"), 0, "{err}");
    assert_eq!(state(dir.path())["sent_today"], json!(0));
    let (code, err) = run(&cfg, &["--once"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(f.calls("z_sendmany"), 1, "{err}");
    assert_eq!(state(dir.path())["sent_today"], json!(2_100_000_000u64));
}

#[test]
fn an_empty_note_snapshot_proves_nothing() {
    let f = healthy_refill_wallet("100", "4");
    let dir = tempfile::tempdir().unwrap();
    interrupted_state(dir.path(), None, true, 1000);
    // Rewrite the snapshot as empty.
    let p = dir.path().join("state/refill.state");
    let text = std::fs::read_to_string(&p)
        .unwrap()
        .replace("[\"n1:ironwood:0\"]", "[]");
    std::fs::write(&p, text).unwrap();
    let (code, err) = run(&refill_config(dir.path(), &f.url), &["--once"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("event=refill_unsettled"), "{err}");
    assert_eq!(f.calls("z_sendmany"), 0);
}

#[test]
fn clear_halt_accepts_the_transfer_and_resumes() {
    // A refill whose check fails halts both commands.
    let f = healthy_refill_wallet("100", "4");
    f.on("getrawtransaction", |_| {
        Reply::Ok(v6_refill_tx("tmSomeoneElse", 2_100_000_000))
    });
    let dir = tempfile::tempdir().unwrap();
    let cfg = refill_config(dir.path(), &f.url);
    let (code, err) = run(&cfg, &["--once"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("event=halted"), "{err}");
    let (code, _) = run(&cfg, &["--once"]);
    assert_eq!(code, 1, "halted");
    // Deleting the marker alone is not enough: the same check would halt again. --clear-halt
    // accepts the transfer and removes the marker.
    let (code, err) = run(&cfg, &["--clear-halt"]);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("accepted_txid=refilltx"), "{err}");
    assert_eq!(state(dir.path())["pending_check"], Value::Null);
    f.on("getrawtransaction", |_| {
        Reply::Ok(v6_refill_tx(DEST, 2_100_000_000))
    });
    let (code, err) = run(&cfg, &["--once"]);
    assert!(code == 0 || code == 2, "{err}");
    assert!(!err.contains("reason=halted"), "{err}");
}

#[test]
fn a_missing_cookie_file_waits_instead_of_refusing() {
    let f = healthy_refill_wallet("100", "4");
    let dir = tempfile::tempdir().unwrap();
    let cfg = refill_config(dir.path(), &f.url);
    let text = std::fs::read_to_string(&cfg).unwrap().replace(
        "user = \"pool\"",
        &format!("cookie_file = \"{}/.cookie\"", dir.path().display()),
    );
    std::fs::write(&cfg, text).unwrap();
    let (code, err) = run(&cfg, &["--once"]);
    assert_eq!(code, 2, "not a refusal: {err}");
    assert!(err.contains("event=startup_waiting"), "{err}");
}

#[test]
fn external_destination_needs_the_double_entry() {
    let f = healthy_refill_wallet("100", "4");
    f.on("validateaddress", |p| {
        let mine = p[0] != DEST;
        Reply::Ok(json!({"isvalid": true, "ismine": mine}))
    });
    let dir = tempfile::tempdir().unwrap();
    let cfg = refill_config(dir.path(), &f.url);
    let text = std::fs::read_to_string(&cfg)
        .unwrap()
        .replace("mode = \"top_up\"", "mode = \"drain_above\"\nreserve = 50")
        .replace("low_water = 10\ntarget = 25\n", "");
    std::fs::write(&cfg, &text).unwrap();
    let (code, err) = run(&cfg, &["--once"]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("external_destination"), "{err}");
    std::fs::write(
        &cfg,
        text.replace(
            "[refill]",
            &format!("[refill]\nexternal_destination = \"{DEST}\""),
        ),
    )
    .unwrap();
    let (code, err) = run(&cfg, &["--once"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        f.params("z_sendmany")[0][1][0]["amount"].to_string(),
        "30.00000000"
    );
}

#[test]
fn a_halted_sweep_also_stops_refill() {
    let f = healthy_refill_wallet("100", "4");
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("state")).unwrap();
    std::fs::write(dir.path().join("state/sweep.halt"), "bad sweep\n").unwrap();
    let (code, err) = run(&refill_config(dir.path(), &f.url), &["--once"]);
    assert_eq!(code, 1, "{err}");
    assert_eq!(f.calls("z_sendmany"), 0);
}

#[test]
fn a_fresh_interrupted_refill_is_never_settled_as_not_sent() {
    // No evidence either way, but the record is only 30 seconds old: the request might still be
    // queued in the wallet, so the tool must wait rather than refund and send again.
    let f = healthy_refill_wallet("100", "4");
    let dir = tempfile::tempdir().unwrap();
    interrupted_state(dir.path(), None, true, 30);
    let (code, err) = run(&refill_config(dir.path(), &f.url), &["--once"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("event=refill_settling"), "{err}");
    assert_eq!(f.calls("z_sendmany"), 0);
}

#[test]
fn coinbase_at_the_destination_does_not_count_as_payable() {
    // Setup 1 in the pool guide: coinbase and payouts share one t-address.
    let f = healthy_refill_wallet("100", "4");
    f.on("listunspent", |p| {
        if p[2][0] == DEST {
            Reply::Ok(json!([
                {"txid": "d0", "vout": 0, "address": DEST, "amount": 4, "confirmations": 5},
                {"txid": "cb", "vout": 0, "address": DEST, "amount": 300, "confirmations": 150,
                 "generated": true}
            ]))
        } else {
            Reply::Ok(json!([]))
        }
    });
    let dir = tempfile::tempdir().unwrap();
    let (code, err) = run(&refill_config(dir.path(), &f.url), &["--once"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        f.params("z_sendmany")[0][1][0]["amount"].to_string(),
        "21.00000000"
    );
}

#[test]
fn a_failed_operation_is_refunded_only_when_no_note_was_spent() {
    let failed = |p: &Value| {
        let id = p[0][0].as_str().unwrap_or("x").to_owned();
        Reply::Ok(json!([{"id": id, "status": "failed",
                          "error": {"code": -4, "message": "commit failed"}}]))
    };
    // Notes untouched: settled as not sent and refunded.
    let f = healthy_refill_wallet("100", "4");
    f.on("z_getoperationstatus", failed);
    let dir = tempfile::tempdir().unwrap();
    let (code, err) = run(&refill_config(dir.path(), &f.url), &["--once"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("event=refill_not_sent"), "{err}");
    assert_eq!(state(dir.path())["sent_today"], json!(0));

    // The note was spent: the wallet may have kept the transaction. Not refunded, not resent.
    let f = healthy_refill_wallet("100", "4");
    f.on("z_getoperationstatus", failed);
    let spent = std::sync::Arc::new(std::sync::Mutex::new(false));
    let s2 = spent.clone();
    f.on("z_listunspent", move |_| {
        if *s2.lock().unwrap() {
            Reply::Ok(json!([]))
        } else {
            Reply::Ok(json!([{"txid": "n1", "pool": "ironwood", "outindex": 0}]))
        }
    });
    let s3 = spent.clone();
    f.on("z_sendmany", move |_| {
        *s3.lock().unwrap() = true;
        Reply::Ok(json!("opid-send"))
    });
    let dir = tempfile::tempdir().unwrap();
    let cfg = refill_config(dir.path(), &f.url);
    let (code, err) = run(&cfg, &["--once"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("event=refill_unsettled"), "{err}");
    assert_eq!(state(dir.path())["sent_today"], json!(2_100_000_000u64));
    let (code, err) = run(&cfg, &["--once"]);
    assert_eq!(code, 2, "{err}");
    assert_eq!(f.calls("z_sendmany"), 1, "never resent");
    // An operator cannot override what the evidence shows.
    let (code, _) = run(&cfg, &["--resolve-interrupted", "not-sent", "--dry-run"]);
    assert_eq!(code, 1, "--dry-run with --resolve-interrupted is refused");
}

#[test]
fn resolve_refuses_a_verdict_the_wallet_contradicts() {
    // The operation shows the transfer was sent; "not-sent" must be refused.
    let f = healthy_refill_wallet("100", "24");
    f.on("z_listoperationids", |_| Reply::Ok(json!(["opid-old"])));
    let dir = tempfile::tempdir().unwrap();
    interrupted_state(dir.path(), Some("opid-old"), true, 30);
    let cfg = refill_config(dir.path(), &f.url);
    let (code, err) = run(&cfg, &["--resolve-interrupted", "not-sent"]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("refusing not-sent"), "{err}");
    assert!(state(dir.path())["in_flight"].is_object());
}

#[test]
fn a_backwards_clock_cannot_reset_the_daily_total() {
    let f = healthy_refill_wallet("100", "4");
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("state")).unwrap();
    // A total recorded for a day far in the future (the clock later stepped back).
    std::fs::write(
        dir.path().join("state/refill.state"),
        json!({"day": 99_999, "sent_today": 9_000_000_000u64, "in_flight": null,
               "unconfirmed": null})
        .to_string(),
    )
    .unwrap();
    let (code, err) = run(&refill_config(dir.path(), &f.url), &["--once"]);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("max_per_day"), "{err}");
    assert_eq!(f.calls("z_sendmany"), 0);
}

#[test]
fn clear_halt_refuses_when_not_halted() {
    // A transfer whose check has not run yet (no halt) must not be waved through.
    let f = healthy_refill_wallet("100", "4");
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("state")).unwrap();
    std::fs::write(
        dir.path().join("state/refill.state"),
        json!({"day": 0, "sent_today": 0, "in_flight": null, "unconfirmed": null,
               "pending_check": {"txid": "t", "amount": 1, "destination": DEST}})
        .to_string(),
    )
    .unwrap();
    let (code, err) = run(&refill_config(dir.path(), &f.url), &["--clear-halt"]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("not halted"), "{err}");
    assert!(state(dir.path())["pending_check"].is_object());
}

// Several destinations.

const A: &str = "tmPayA";
const B: &str = "tmPayB";
const C: &str = "tmPayC";

/// What each txid paid: (address, zatoshis).
type Built = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, (String, u64)>>>;

/// A refill wallet whose owned t-addresses hold `balances`. Each z_sendmany gets its own
/// operation and txid, and the transaction it builds pays the address and amount it was asked.
fn multi_wallet(spendable: &str, balances: &[(&str, &str)]) -> (FakeRpc, Built) {
    let f = healthy_refill_wallet(spendable, "0");
    let balances: Vec<(String, Value)> = balances
        .iter()
        .map(|(a, b)| (a.to_string(), serde_json::from_str(b).unwrap()))
        .collect();
    f.on("listunspent", move |p| {
        let found = balances.iter().find(|(a, _)| p[2][0] == json!(a));
        Reply::Ok(match found {
            Some((a, b)) => json!([{"txid": format!("d-{a}"), "vout": 0, "address": a,
                                    "amount": b, "confirmations": 5}]),
            None => json!([]),
        })
    });
    let built: Built = Default::default();
    let b2 = built.clone();
    f.on("z_sendmany", move |p| {
        let mut b = b2.lock().unwrap();
        let opid = format!("opid-{}", b.len() + 1);
        let zat = coinbase_sweeper::amount::Zat::from_json(&p[1][0]["amount"])
            .unwrap()
            .0;
        let addr = p[1][0]["address"].as_str().unwrap().to_owned();
        b.insert(format!("tx-{opid}"), (addr, zat));
        Reply::Ok(json!(opid))
    });
    f.on("z_getoperationstatus", |p| {
        let id = p[0][0].as_str().unwrap_or("x").to_owned();
        Reply::Ok(
            json!([{"id": id, "status": "success", "method": "z_sendmany",
                          "result": {"txid": format!("tx-{id}")}}]),
        )
    });
    let b3 = built.clone();
    f.on("getrawtransaction", move |p| {
        let b = b3.lock().unwrap();
        let (addr, zat) = &b[p[0].as_str().unwrap()];
        Reply::Ok(v6_refill_tx(addr, *zat))
    });
    (f, built)
}

/// The default refill config with its `[refill]` section replaced.
fn multi_config(dir: &std::path::Path, url: &str, section: &str) -> std::path::PathBuf {
    let cfg = refill_config(dir, url);
    let text = std::fs::read_to_string(&cfg).unwrap();
    let head = text.split("[refill]").next().unwrap();
    std::fs::write(&cfg, format!("{head}[refill]\n{section}")).unwrap();
    cfg
}

fn drain_above(destinations: &str) -> String {
    format!(
        "mode = \"drain_above\"\n{destinations}\nreserve = 50\nmax_per_transfer = 5\n\
         max_per_day = 1000\n"
    )
}

fn top_up(destinations: &str) -> String {
    format!(
        "mode = \"top_up\"\n{destinations}\nlow_water = 10\ntarget = 25\n\
         max_per_transfer = 30\nmax_per_day = 100\n"
    )
}

fn paid_addresses(f: &FakeRpc) -> Vec<String> {
    f.params("z_sendmany")
        .iter()
        .map(|p| p[1][0]["address"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn drain_above_rotates_one_destination_per_transfer() {
    let (f, _) = multi_wallet("100", &[]);
    let dir = tempfile::tempdir().unwrap();
    let cfg = multi_config(
        dir.path(),
        &f.url,
        &drain_above(&format!("destinations = [\"{A}\", \"{B}\", \"{C}\"]")),
    );
    for i in 0..4 {
        let (code, err) = run(&cfg, &["--once"]);
        assert_eq!(code, 0, "run {i}: {err}");
    }
    assert_eq!(paid_addresses(&f), [A, B, C, A], "wraps around");
    for p in f.params("z_sendmany") {
        assert_eq!(
            p[1].as_array().unwrap().len(),
            1,
            "one destination per transfer"
        );
        assert_eq!(p[1][0]["amount"].to_string(), "5.00000000");
    }
    let s = state(dir.path());
    assert_eq!(s["last_destination"], json!(A));
    assert_eq!(
        s["sent_today"],
        json!(2_000_000_000u64),
        "one shared daily total"
    );

    // A transfer the wallet could not fund was not sent: the turn does not move on.
    f.on("z_sendmany", |_| rpc_err(-6, "Insufficient funds"));
    let (code, err) = run(&cfg, &["--once"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        err.contains(&format!(
            "addr={B} amount=5.00000000 reason=insufficient_funds"
        )),
        "{err}"
    );
    assert_eq!(state(dir.path())["last_destination"], json!(A));
}

#[test]
fn the_daily_cap_is_shared_by_all_destinations() {
    let (f, _) = multi_wallet("100", &[]);
    let dir = tempfile::tempdir().unwrap();
    let cfg = multi_config(
        dir.path(),
        &f.url,
        &drain_above(&format!("destinations = [\"{A}\", \"{B}\", \"{C}\"]"))
            .replace("max_per_day = 1000", "max_per_day = 10"),
    );
    for _ in 0..2 {
        let (code, err) = run(&cfg, &["--once"]);
        assert_eq!(code, 0, "{err}");
    }
    let (code, err) = run(&cfg, &["--once"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(paid_addresses(&f), [A, B]);
    assert!(err.contains("max_per_day"), "{err}");
    assert!(
        err.contains(&format!("event=refill_skip cmd=refill addr={C}")),
        "{err}"
    );
}

#[test]
fn a_single_destination_behaves_as_before() {
    let (f, _) = multi_wallet("100", &[]);
    let dir = tempfile::tempdir().unwrap();
    let cfg = multi_config(
        dir.path(),
        &f.url,
        &drain_above(&format!("destination = \"{A}\"")),
    );
    for _ in 0..3 {
        let (code, err) = run(&cfg, &["--once"]);
        assert_eq!(code, 0, "{err}");
    }
    assert_eq!(paid_addresses(&f), [A, A, A]);
    let p = &f.params("z_sendmany")[0];
    assert_eq!(p[0], json!(UA));
    assert_eq!(p[2], json!(10));
    assert_eq!(p[3], Value::Null);
    assert_eq!(p[4], json!("AllowRevealedRecipients"));
}

#[test]
fn top_up_refills_the_emptiest_destination_below_low_water() {
    let (f, _) = multi_wallet("100", &[(A, "8"), (B, "3"), (C, "12")]);
    let dir = tempfile::tempdir().unwrap();
    let section = top_up(&format!("destinations = [\"{A}\", \"{B}\", \"{C}\"]"));
    let cfg = multi_config(dir.path(), &f.url, &section);
    let (code, err) = run(&cfg, &["--once"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(paid_addresses(&f), [B]);
    assert_eq!(
        f.params("z_sendmany")[0][1][0]["amount"].to_string(),
        "22.00000000",
        "up to target"
    );
    assert_eq!(
        f.calls("listunspent"),
        3,
        "each destination's balance was read"
    );

    // Every destination at or above low_water: nothing is sent.
    let (f, _) = multi_wallet("100", &[(A, "18"), (B, "10"), (C, "12")]);
    let dir = tempfile::tempdir().unwrap();
    let cfg = multi_config(dir.path(), &f.url, &section);
    let (code, err) = run(&cfg, &["--once"]);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains(&format!("addr={B}")), "{err}");
    assert!(err.contains("not below low_water"), "{err}");
    assert_eq!(f.calls("z_sendmany"), 0);
}

#[test]
fn an_external_destination_in_a_list_needs_the_double_entry() {
    let (f, _) = multi_wallet("100", &[]);
    f.on("validateaddress", |p| {
        Reply::Ok(json!({"isvalid": true, "ismine": p[0] != json!(B)}))
    });
    let dir = tempfile::tempdir().unwrap();
    let list = format!("destinations = [\"{A}\", \"{B}\"]");
    let cfg = multi_config(dir.path(), &f.url, &drain_above(&list));
    let (code, err) = run(&cfg, &["--once"]);
    assert_eq!(code, 1, "{err}");
    assert!(
        err.contains(&format!("external_destinations = [\"{B}\"]")),
        "{err}"
    );
    let cfg = multi_config(
        dir.path(),
        &f.url,
        &drain_above(&format!("{list}\nexternal_destinations = [\"{B}\"]")),
    );
    let (code, err) = run(&cfg, &["--once"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(paid_addresses(&f), [A]);
}

#[test]
fn an_in_flight_transfer_to_a_removed_destination_still_settles() {
    // A transfer to DEST was interrupted; the operator then replaced DEST by A and B.
    let (f, built) = multi_wallet("100", &[]);
    built
        .lock()
        .unwrap()
        .insert("tx-opid-old".into(), (DEST.into(), 2_000_000_000));
    f.on("z_listoperationids", |_| Reply::Ok(json!(["opid-old"])));
    let dir = tempfile::tempdir().unwrap();
    interrupted_state(dir.path(), Some("opid-old"), true, 30);
    let cfg = multi_config(
        dir.path(),
        &f.url,
        &drain_above(&format!("destinations = [\"{A}\", \"{B}\"]")),
    );
    let (code, err) = run(&cfg, &["--once"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        err.contains(&format!(
            "event=refilled cmd=refill addr={DEST} amount=20.00000000 txid=tx-opid-old"
        )),
        "settled against the recorded destination: {err}"
    );
    assert!(err.contains("outcome=ok_recovered"), "{err}");
    assert!(!err.contains("event=halted"), "checked against DEST: {err}");
    // The turn starts over at the first configured destination.
    assert_eq!(paid_addresses(&f), [A]);
    let s = state(dir.path());
    assert_eq!(s["in_flight"], Value::Null);
    assert_eq!(s["pending_check"], Value::Null);
    assert_eq!(s["last_destination"], json!(A));
    assert_eq!(
        s["sent_today"],
        json!(2_500_000_000u64),
        "20 recovered + 5 new"
    );
}

#[test]
fn an_operator_verdict_of_sent_moves_the_turn_on() {
    // An interrupted transfer to B that the wallet cannot account for: source notes are gone,
    // but no operation or history entry shows it. The operator checks B and says "sent".
    let (f, _) = multi_wallet("100", &[]);
    f.on("z_listunspent", |_| Reply::Ok(json!([])));
    let dir = tempfile::tempdir().unwrap();
    interrupted_state(dir.path(), None, true, 1000);
    let p = dir.path().join("state/refill.state");
    let text = std::fs::read_to_string(&p).unwrap().replace(DEST, B);
    std::fs::write(&p, text).unwrap();
    let cfg = multi_config(
        dir.path(),
        &f.url,
        &drain_above(&format!("destinations = [\"{A}\", \"{B}\", \"{C}\"]")),
    );
    let (code, err) = run(&cfg, &["--once"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("event=refill_unsettled"), "{err}");
    let (code, err) = run(&cfg, &["--resolve-interrupted", "sent"]);
    assert_eq!(code, 0, "{err}");
    let s = state(dir.path());
    assert_eq!(s["last_destination"], json!(B));
    assert_eq!(s["sent_today"], json!(2_000_000_000u64), "kept");
    let (code, err) = run(&cfg, &["--once"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(paid_addresses(&f), [C], "B is not paid twice in a row");
}
