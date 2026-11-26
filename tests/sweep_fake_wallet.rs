//! The RPC client and the `sweep` command against a scripted wallet.

mod common;

use std::time::Duration;

use coinbase_sweeper::config::Credentials;
use coinbase_sweeper::rpc::{Rpc, RpcError};
use coinbase_sweeper::state::StateDir;
use common::fake_rpc::*;
use serde_json::json;

fn client(url: &str, timeout: Duration) -> Rpc {
    let cred = Credentials {
        user: "pool".into(),
        password: "pw".into(),
    };
    Rpc::new(url, &cred, None, timeout, timeout, 2)
}

#[test]
fn http_500_with_json_error_body() {
    let f = FakeRpc::start();
    f.on("listunspent", |_| rpc_err(-8, "Invalid parameter"));
    let e = client(&f.url, Duration::from_secs(2))
        .read("listunspent", json!([]))
        .unwrap_err();
    assert_eq!(
        e,
        RpcError::Rpc {
            code: -8,
            message: "Invalid parameter".into()
        }
    );
    // An RPC error is an answer: it is not retried.
    assert_eq!(f.calls("listunspent"), 1);
}

#[test]
fn http_401_is_an_auth_error_and_refuses_startup() {
    let f = FakeRpc::start();
    f.on("getblockchaininfo", |_| Reply::Status(401));
    f.on("z_listunifiedreceivers", |_| Reply::Status(401));
    let e = client(&f.url, Duration::from_secs(2))
        .read("getblockchaininfo", json!([]))
        .unwrap_err();
    assert_eq!(e, RpcError::Auth(401));
    let dir = tempfile::tempdir().unwrap();
    let cfg = sweep_config(dir.path(), &f.url, "");
    let (code, err) = run_bin(&["sweep", "--config", cfg.to_str().unwrap(), "--once"]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("credentials"), "{err}");
}

#[test]
fn dropped_connection_on_a_read_is_retried() {
    let f = FakeRpc::start();
    let mut first = true;
    f.on("getblockchaininfo", move |_| {
        if std::mem::take(&mut first) {
            Reply::Drop
        } else {
            Reply::Ok(json!({"blocks": 1, "headers": 1}))
        }
    });
    let v = client(&f.url, Duration::from_secs(2))
        .read("getblockchaininfo", json!([]))
        .unwrap();
    assert_eq!(v["blocks"], 1);
    assert_eq!(f.calls("getblockchaininfo"), 2);
}

#[test]
fn minus_six_means_nothing_to_shield() {
    let f = healthy_sweep_wallet(3);
    f.on("z_shieldcoinbase", |_| {
        rpc_err(-6, "Could not find any coinbase funds to shield.")
    });
    let dir = tempfile::tempdir().unwrap();
    let cfg = sweep_config(dir.path(), &f.url, "");
    let (code, err) = run_bin(&["sweep", "--config", cfg.to_str().unwrap(), "--once"]);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("event=nothing_to_shield"), "{err}");
    assert_eq!(f.calls("z_shieldcoinbase"), 1);
}

#[test]
fn sweeps_in_batches_of_fifty_with_a_confirmation_between() {
    let f = healthy_sweep_wallet(120);
    let dir = tempfile::tempdir().unwrap();
    let cfg = sweep_config(dir.path(), &f.url, "");
    let (code, err) = run_bin(&["sweep", "--config", cfg.to_str().unwrap(), "--once"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(f.calls("z_shieldcoinbase"), 3);
    for p in f.params("z_shieldcoinbase") {
        assert_eq!(p, json!([TADDR, UA, null, 50]));
    }
    // A confirmation wait after the first two batches, none after the last.
    assert_eq!(err.matches("event=wait_confirmation").count(), 2, "{err}");
    assert_eq!(err.matches("event=swept").count(), 3, "{err}");
}

#[test]
fn timed_out_send_is_not_resent() {
    let f = healthy_sweep_wallet(10);
    f.on("z_shieldcoinbase", |_| {
        Reply::Hang(
            Duration::from_secs(4),
            Box::new(Reply::Ok(json!({"opid": "opid-late", "shieldingUTXOs": 10,
                                      "remainingUTXOs": 0}))),
        )
    });
    let dir = tempfile::tempdir().unwrap();
    let cfg = sweep_config(dir.path(), &f.url, "");
    let (code, err) = run_bin(&["sweep", "--config", cfg.to_str().unwrap(), "--once"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("event=submit_unknown"), "{err}");
    assert_eq!(f.calls("z_shieldcoinbase"), 1, "{err}");
}

#[test]
fn startup_refusals() {
    let dir = tempfile::tempdir().unwrap();
    let refused = |f: &FakeRpc, needle: &str| {
        let cfg = sweep_config(dir.path(), &f.url, "");
        let (code, err) = run_bin(&["sweep", "--config", cfg.to_str().unwrap(), "--once"]);
        assert_eq!(code, 1, "{needle}: {err}");
        assert!(err.contains(needle), "{needle}: {err}");
        assert_eq!(f.calls("z_shieldcoinbase"), 0);
    };

    let f = healthy_sweep_wallet(10);
    f.on("z_listunifiedreceivers", |_| {
        Reply::Ok(json!({"sapling": "zs", "orchard": "x"}))
    });
    refused(&f, "Sapling receiver");

    let f = healthy_sweep_wallet(10);
    f.on("z_listunifiedreceivers", |_| {
        Reply::Ok(json!({"p2pkh": "t"}))
    });
    refused(&f, "no Orchard receiver");

    let f = healthy_sweep_wallet(10);
    f.on("z_validateaddress", |_| {
        Reply::Ok(json!({"isvalid": true, "ismine": false}))
    });
    refused(&f, "does not belong to this wallet");

    let f = healthy_sweep_wallet(10);
    f.on("validateaddress", |_| {
        Reply::Ok(json!({"isvalid": true, "ismine": false}))
    });
    refused(&f, "cannot sign for");

    let f = healthy_sweep_wallet(10);
    f.on("validateaddress", |_| Reply::Ok(json!({"isvalid": false})));
    refused(&f, "not a transparent address");

    // A second instance.
    let f = healthy_sweep_wallet(10);
    let state = StateDir::new(
        &dir.path().join("state"),
        coinbase_sweeper::config::Command::Sweep,
    )
    .unwrap();
    let held = state.lock().unwrap();
    refused(&f, "already running");
    drop(held);

    // Configuration errors.
    let cfg = dir.path().join("bad.toml");
    std::fs::write(
        &cfg,
        "shielded_address = \"u\"\n[rpc]\nurl = \"https://x\"\n",
    )
    .unwrap();
    let (code, err) = run_bin(&["sweep", "--config", cfg.to_str().unwrap(), "--once"]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("http://"), "{err}");
}

#[test]
fn bad_sweep_transaction_halts_until_cleared() {
    let f = healthy_sweep_wallet(10);
    f.on("getrawtransaction", |_| {
        Reply::Ok(json!({"version": 5, "vin": [{}], "vout": [],
                         "vShieldedSpend": [], "vShieldedOutput": [{}]}))
    });
    let dir = tempfile::tempdir().unwrap();
    let cfg = sweep_config(dir.path(), &f.url, "");
    let args = ["sweep", "--config", cfg.to_str().unwrap(), "--once"];
    let (code, err) = run_bin(&args);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("event=halted"), "{err}");
    let (code, err) = run_bin(&args);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("reason=halted"), "{err}");
    assert_eq!(f.calls("z_shieldcoinbase"), 1);
}

#[test]
fn skipped_cycles_when_syncing_or_behind() {
    let f = healthy_sweep_wallet(10);
    f.on("getblockchaininfo", |_| {
        Reply::Ok(json!({"blocks": 400, "headers": 500, "initialblockdownload": false}))
    });
    let dir = tempfile::tempdir().unwrap();
    let cfg = sweep_config(dir.path(), &f.url, "");
    let (code, err) = run_bin(&["sweep", "--config", cfg.to_str().unwrap(), "--once"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("blocks behind"), "{err}");
    assert_eq!(f.calls("z_shieldcoinbase"), 0);
}

#[test]
fn node_rejection_pauses_the_command() {
    let f = healthy_sweep_wallet(10);
    f.on("z_getoperationstatus", |p| {
        let id = p[0][0].as_str().unwrap().to_owned();
        Reply::Ok(json!([{"id": id, "status": "failed",
                          "error": {"code": -26, "message": "bad-txns-sapling-output"}}]))
    });
    let dir = tempfile::tempdir().unwrap();
    let cfg = sweep_config(dir.path(), &f.url, "rejection_pause_secs = 600");
    let args = ["sweep", "--config", cfg.to_str().unwrap(), "--once"];
    let (code, err) = run_bin(&args);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("event=node_rejected"), "{err}");
    let (code, err) = run_bin(&args);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("event=paused"), "{err}");
    assert_eq!(f.calls("z_shieldcoinbase"), 1);
}

#[test]
fn dry_run_and_check_send_nothing() {
    let f = healthy_sweep_wallet(120);
    let dir = tempfile::tempdir().unwrap();
    let cfg = sweep_config(dir.path(), &f.url, "");
    let (code, err) = run_bin(&["sweep", "--config", cfg.to_str().unwrap(), "--dry-run"]);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("would_shield_inputs=120 batches=3"), "{err}");
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_coinbase-sweeper"))
        .args(["sweep", "--config", cfg.to_str().unwrap(), "--check"])
        .env("SWEEPER_RPC_PASSWORD", "pw")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{stdout}");
    assert!(stdout.contains("mature_unswept=120"), "{stdout}");
    assert_eq!(f.calls("z_shieldcoinbase"), 0);
}

#[test]
fn bad_gateway_on_a_send_is_not_resent() {
    let f = healthy_sweep_wallet(10);
    f.on("z_shieldcoinbase", |_| Reply::Status(502));
    let dir = tempfile::tempdir().unwrap();
    let cfg = sweep_config(dir.path(), &f.url, "");
    let (code, err) = run_bin(&["sweep", "--config", cfg.to_str().unwrap(), "--once"]);
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("event=submit_unknown"), "{err}");
    assert_eq!(f.calls("z_shieldcoinbase"), 1);
}

#[test]
fn cookie_is_reread_after_401() {
    let f = healthy_sweep_wallet(0);
    let dir = tempfile::tempdir().unwrap();
    let cookie = dir.path().join(".cookie");
    std::fs::write(&cookie, "__cookie__:old").unwrap();
    let cred = coinbase_sweeper::config::read_cookie(&cookie).unwrap();
    let rpc = Rpc::new(
        &f.url,
        &cred,
        Some(cookie.clone()),
        Duration::from_secs(2),
        Duration::from_secs(2),
        0,
    );
    // The wallet restarted and wrote a new cookie: the old one is refused once.
    let mut first = true;
    f.on("getblockchaininfo", move |_| {
        if std::mem::take(&mut first) {
            Reply::Status(401)
        } else {
            Reply::Ok(json!({"blocks": 1, "headers": 1}))
        }
    });
    std::fs::write(&cookie, "__cookie__:new").unwrap();
    rpc.read("getblockchaininfo", json!([])).unwrap();
    assert_eq!(f.calls("getblockchaininfo"), 2);
}

#[test]
fn other_programs_operations_are_waited_for_but_not_cleared() {
    let f = healthy_sweep_wallet(3);
    f.on("z_listoperationids", |_| Reply::Ok(json!(["opid-payout"])));
    f.on("z_getoperationstatus", |p| {
        let id = p[0][0].as_str().unwrap().to_owned();
        if id == "opid-payout" {
            return Reply::Ok(
                json!([{"id": id, "status": "success", "method": "z_sendmany",
                                      "params": {"fromaddress": "tmPayout"},
                                      "result": {"txid": "payout"}}]),
            );
        }
        Reply::Ok(
            json!([{"id": id, "status": "success", "method": "z_shieldcoinbase",
                          "result": {"txid": format!("tx-{id}")}}]),
        )
    });
    let dir = tempfile::tempdir().unwrap();
    let cfg = sweep_config(dir.path(), &f.url, "");
    let (code, err) = run_bin(&["sweep", "--config", cfg.to_str().unwrap(), "--once"]);
    assert_eq!(code, 0, "{err}");
    for p in f.params("z_getoperationresult") {
        assert_ne!(
            p,
            json!([["opid-payout"]]),
            "cleared another program's result"
        );
    }
}

#[test]
fn an_expired_previous_sweep_does_not_wedge_the_command() {
    let f = healthy_sweep_wallet(3);
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("sweep.state"),
        r#"{"unconfirmed": [{"txid": "old", "address": "tmPool", "checked": true}]}"#,
    )
    .unwrap();
    f.on("gettransaction", |p| {
        if p[0] == "old" {
            Reply::Ok(json!({"confirmations": 0, "expiryheight": 450}))
        } else {
            Reply::Ok(json!({"confirmations": 1}))
        }
    });
    let cfg = sweep_config(dir.path(), &f.url, "");
    let (code, err) = run_bin(&["sweep", "--config", cfg.to_str().unwrap(), "--once"]);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("event=sweep_gone"), "{err}");
    assert_eq!(f.calls("z_shieldcoinbase"), 1);
}
