//! A tiny scripted JSON-RPC server on a local port. Each method has a handler that decides the
//! reply; every request is recorded so tests can count calls.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

pub enum Reply {
    Ok(Value),
    /// A JSON-RPC error sent with the given HTTP status (zcashd uses 500).
    Err {
        status: u16,
        code: i64,
        message: String,
    },
    /// A bare HTTP status with no JSON body.
    Status(u16),
    /// Close the connection without answering.
    Drop,
    /// Answer only after this delay.
    Hang(Duration, Box<Reply>),
}

pub fn rpc_err(code: i64, message: &str) -> Reply {
    Reply::Err {
        status: 500,
        code,
        message: message.into(),
    }
}

type Handler = Box<dyn FnMut(&Value) -> Reply + Send>;

#[derive(Clone)]
pub struct FakeRpc {
    pub url: String,
    handlers: Arc<Mutex<HashMap<String, Handler>>>,
    calls: Arc<Mutex<Vec<(String, Value)>>>,
}

impl FakeRpc {
    pub fn start() -> FakeRpc {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let fake = FakeRpc {
            url,
            handlers: Arc::default(),
            calls: Arc::default(),
        };
        let f = fake.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let f = f.clone();
                std::thread::spawn(move || f.serve(stream));
            }
        });
        fake
    }

    pub fn on(&self, method: &str, h: impl FnMut(&Value) -> Reply + Send + 'static) {
        self.handlers
            .lock()
            .unwrap()
            .insert(method.to_owned(), Box::new(h));
    }

    pub fn calls(&self, method: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| m == method)
            .count()
    }

    pub fn params(&self, method: &str) -> Vec<Value> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, _)| m == method)
            .map(|(_, p)| p.clone())
            .collect()
    }

    fn serve(&self, stream: TcpStream) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut stream = stream;
        loop {
            let mut len = 0usize;
            let mut line = String::new();
            let mut first = true;
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                if first {
                    first = false;
                }
                let l = line.trim_end();
                if l.is_empty() {
                    break;
                }
                if let Some(v) = l
                    .to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().to_owned())
                {
                    len = v.parse().unwrap();
                }
            }
            let mut body = vec![0; len];
            if reader.read_exact(&mut body).is_err() {
                return;
            }
            let req: Value = serde_json::from_slice(&body).unwrap();
            let method = req["method"].as_str().unwrap().to_owned();
            let params = req["params"].clone();
            self.calls
                .lock()
                .unwrap()
                .push((method.clone(), params.clone()));
            let reply = {
                let mut hs = self.handlers.lock().unwrap();
                match hs.get_mut(&method) {
                    Some(h) => h(&params),
                    None => rpc_err(-32601, &format!("Method not found: {method}")),
                }
            };
            if !self.respond(&mut stream, reply, &req["id"]) {
                return;
            }
        }
    }

    fn respond(&self, stream: &mut TcpStream, reply: Reply, id: &Value) -> bool {
        let (status, body) = match reply {
            Reply::Ok(v) => (
                200,
                json!({"result": v, "error": null, "id": id}).to_string(),
            ),
            Reply::Err {
                status,
                code,
                message,
            } => (
                status,
                json!({"result": null, "error": {"code": code, "message": message}, "id": id})
                    .to_string(),
            ),
            Reply::Status(s) => (s, String::new()),
            Reply::Drop => {
                let _ = stream.shutdown(std::net::Shutdown::Both);
                return false;
            }
            Reply::Hang(d, inner) => {
                std::thread::sleep(d);
                return self.respond(stream, *inner, id);
            }
        };
        let head = format!(
            "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).is_ok() && stream.write_all(body.as_bytes()).is_ok()
    }
}

pub const UA: &str = "uregtest1pool";
pub const TADDR: &str = "tmPool";

/// A wallet that answers every call a healthy sweep cycle makes, with `mature` mature coinbase
/// outputs at `TADDR`. Each z_shieldcoinbase takes up to 50 and produces a valid v6 sweep.
pub fn healthy_sweep_wallet(mature: usize) -> FakeRpc {
    let f = FakeRpc::start();
    f.on("getconnectioncount", |_| Reply::Ok(json!(1)));
    f.on("getblockchaininfo", |_| {
        Reply::Ok(json!({"blocks": 500, "headers": 500, "initialblockdownload": false}))
    });
    f.on("z_listunifiedreceivers", |_| {
        Reply::Ok(json!({"orchard": "x"}))
    });
    f.on("z_validateaddress", |_| {
        Reply::Ok(json!({"isvalid": true, "ismine": true}))
    });
    f.on("validateaddress", |_| {
        Reply::Ok(json!({"isvalid": true, "ismine": true}))
    });
    f.on("z_listoperationids", |_| Reply::Ok(json!([])));
    f.on("z_getoperationstatus", |p| {
        let id = p[0][0].as_str().unwrap_or("opid-x").to_owned();
        Reply::Ok(
            json!([{"id": id, "status": "success", "method": "z_shieldcoinbase",
                          "result": {"txid": format!("tx-{id}")}}]),
        )
    });
    f.on("z_getoperationresult", |_| Reply::Ok(json!([])));
    let remaining = Arc::new(Mutex::new(mature));
    let r = remaining.clone();
    f.on("listunspent", move |_| {
        let n = *r.lock().unwrap();
        let v: Vec<Value> = (0..n)
            .map(|i| {
                json!({"txid": format!("cb{i}"), "vout": 0, "address": TADDR,
                            "amount": 3.125, "confirmations": 150 + i, "generated": true})
            })
            .collect();
        Reply::Ok(Value::Array(v))
    });
    let counter = Arc::new(Mutex::new(0));
    f.on("z_shieldcoinbase", move |_| {
        let mut n = remaining.lock().unwrap();
        if *n == 0 {
            return rpc_err(-6, "Could not find any coinbase funds to shield.");
        }
        let take = (*n).min(50);
        *n -= take;
        let mut c = counter.lock().unwrap();
        *c += 1;
        Reply::Ok(json!({"opid": format!("opid-{c}"), "shieldingUTXOs": take,
                         "shieldingValue": 3.125, "remainingUTXOs": *n, "remainingValue": 0}))
    });
    f.on("getrawtransaction", |_| Reply::Ok(v6_sweep_tx()));
    f.on("gettransaction", |_| Reply::Ok(json!({"confirmations": 1})));
    f.on("z_getbalance", |_| Reply::Ok(json!(0)));
    f
}

pub fn v6_sweep_tx() -> Value {
    json!({
        "version": 6, "vin": [{"txid": "cb0", "vout": 0}], "vout": [],
        "vShieldedSpend": [], "vShieldedOutput": [],
        "orchard": {"actions": [], "valueBalanceZat": 0},
        "ironwood": {"actions": [{}, {}], "valueBalanceZat": -312500000}
    })
}

/// Writes a sweep config pointing at `url` into `dir` and returns its path.
pub fn sweep_config(dir: &std::path::Path, url: &str, extra_general: &str) -> std::path::PathBuf {
    let text = format!(
        r#"shielded_address = "{UA}"
[rpc]
url = "{url}"
user = "pool"
timeout_secs = 2
submit_timeout_secs = 2
read_retries = 2
[general]
state_dir = "{}"
interval_secs = 1
max_backoff_secs = 2
operation_timeout_secs = 10
confirmation_timeout_secs = 10
{extra_general}
[sweep]
coinbase_addresses = ["{TADDR}"]
min_inputs = 1
alert_unswept_zec = 1000
"#,
        dir.join("state").display()
    );
    let p = dir.join("sweeper.toml");
    std::fs::write(&p, text).unwrap();
    p
}

/// Runs the binary with the given arguments and returns its exit code and stderr.
pub fn run_bin(args: &[&str]) -> (i32, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_coinbase-sweeper"))
        .args(args)
        .env("SWEEPER_RPC_PASSWORD", "pw")
        .env("RUST_LOG", "debug")
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

pub const DEST: &str = "tmDest";

/// A wallet for `refill`: the pool UA holds `spendable`, the owned t-address `DEST` holds
/// `dest_balance`, and each z_sendmany succeeds with a v6 Ironwood transaction paying DEST.
pub fn healthy_refill_wallet(spendable: &str, dest_balance: &str) -> FakeRpc {
    let f = healthy_sweep_wallet(0);
    let spendable: Value = serde_json::from_str(spendable).unwrap();
    f.on("z_getbalance", move |_| Reply::Ok(spendable.clone()));
    let dest_balance: Value = serde_json::from_str(dest_balance).unwrap();
    f.on("listunspent", move |p| {
        if p[2][0] == DEST {
            Reply::Ok(
                json!([{"txid": "d0", "vout": 0, "address": DEST, "amount": dest_balance,
                              "confirmations": 5}]),
            )
        } else {
            Reply::Ok(json!([]))
        }
    });
    // The pool address holds one note; tests override this to show it spent.
    f.on("z_listunspent", |_| {
        Reply::Ok(json!([{"txid": "n1", "pool": "ironwood", "outindex": 0, "amount": 100}]))
    });
    f.on("listtransactions", |_| Reply::Ok(json!([])));
    let sent = Arc::new(Mutex::new(0u64));
    let s2 = sent.clone();
    f.on("z_sendmany", move |p| {
        let zat = coinbase_sweeper::amount::Zat::from_json(&p[1][0]["amount"])
            .unwrap()
            .0;
        *s2.lock().unwrap() = zat;
        Reply::Ok(json!("opid-send"))
    });
    f.on("z_getoperationstatus", |p| {
        let id = p[0][0].as_str().unwrap_or("x").to_owned();
        Reply::Ok(
            json!([{"id": id, "status": "success", "method": "z_sendmany",
                          "result": {"txid": "refilltx"}}]),
        )
    });
    f.on("getrawtransaction", move |_| {
        let zat = *sent.lock().unwrap();
        Reply::Ok(v6_refill_tx(DEST, zat))
    });
    f
}

pub fn v6_refill_tx(dest: &str, zat: u64) -> Value {
    json!({
        "version": 6, "vin": [],
        "vout": [{"valueZat": zat, "scriptPubKey": {"addresses": [dest]}}],
        "vShieldedSpend": [], "vShieldedOutput": [],
        "orchard": {"actions": [], "valueBalanceZat": 0},
        "ironwood": {"actions": [{}, {}], "valueBalanceZat": zat + 15000}
    })
}

/// Writes a top_up refill config (low_water 10, target 25) into `dir`.
pub fn refill_config(dir: &std::path::Path, url: &str) -> std::path::PathBuf {
    let text = format!(
        r#"shielded_address = "{UA}"
[rpc]
url = "{url}"
user = "pool"
timeout_secs = 2
submit_timeout_secs = 2
read_retries = 1
[general]
state_dir = "{}"
interval_secs = 1
max_backoff_secs = 2
operation_timeout_secs = 10
confirmation_timeout_secs = 10
[refill]
mode = "top_up"
destination = "{DEST}"
low_water = 10
target = 25
max_per_transfer = 30
max_per_day = 100
"#,
        dir.join("state").display()
    );
    let p = dir.join("refill.toml");
    std::fs::write(&p, text).unwrap();
    p
}

/// Writes a refill state file holding an interrupted transfer of 20 ZEC to DEST.
/// `with_notes`: the record holds the note snapshot taken before the send.
pub fn interrupted_state(dir: &std::path::Path, opid: Option<&str>, with_notes: bool, age: u64) {
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let s = json!({
        "day": now / 86_400,
        "sent_today": 2_000_000_000u64,
        "in_flight": {
            "started_unix": now - age,
            "day": now / 86_400,
            "amount": 2_000_000_000u64,
            "destination": DEST,
            "opid": opid,
            "ops_before": [],
            "notes_before": if with_notes { json!(["n1:ironwood:0"]) } else { Value::Null }
        },
        "unconfirmed": null
    });
    std::fs::write(state.join("refill.state"), s.to_string()).unwrap();
}
