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
    // Ten minutes on, no operation, no history entry, and the note the source held before the
    // send is still unspent: no funds moved.
    let f = healthy_refill_wallet("100", "4");
    let dir = tempfile::tempdir().unwrap();
    interrupted_state(dir.path(), None, true, 1000);
    let (code, err) = run(&refill_config(dir.path(), &f.url), &["--once"]);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("event=refill_not_sent"), "{err}");
    assert_eq!(f.calls("z_sendmany"), 1, "{err}");
    assert_eq!(state(dir.path())["sent_today"], json!(2_100_000_000u64));
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
