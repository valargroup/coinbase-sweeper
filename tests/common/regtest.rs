//! Shared regtest setup: a Zakura node mining to the pool's transparent address and a zecd wallet
//! restored from the pool's mnemonic, plus helpers to run the tool against them.

#![allow(dead_code)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use zecd_regtest_harness::{
    derive_address_offline, pick_port, resolve_node_bin, Zebrad, Zecd, ZecdConfig,
};

/// The pool wallet, from zecd's committed throwaway regtest phrase.
pub const POOL_MNEMONIC: &str = "mechanic vehicle helmet decide plug gorilla frost dial october \
midnight culture idea mountain fame park social drip bid doctor scatter glance defy moment stage";
/// Another wallet's phrase, for an address this wallet does not own.
pub const OTHER_MNEMONIC: &str = "abandon abandon abandon abandon abandon abandon abandon abandon \
abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon \
abandon abandon abandon art";

pub const SEED_BLOCKS: u32 = 2;
pub const BIRTHDAY: u32 = SEED_BLOCKS + 1;
pub const SYNC_TIMEOUT: Duration = Duration::from_secs(300);

pub struct Pool {
    pub zebrad: Zebrad,
    pub zecd: Zecd,
    pub zecd_cfg: ZecdConfig,
    pub taddr: String,
    pub ua: String,
    pub dir: tempfile::TempDir,
}

/// Returns `None` (after printing why) when the regtest binaries are not configured.
pub fn node_bin(test: &str) -> Option<PathBuf> {
    match resolve_node_bin() {
        Some(b) => Some(b),
        None => {
            eprintln!("SKIP {test}: set ZECD_REGTEST_NODE=zakura, ZAKURAD_BIN and ZECD_BIN");
            None
        }
    }
}

impl Pool {
    /// Starts the node mining to the pool's t-address and the pool's wallet.
    pub async fn start(node_bin: &Path) -> Pool {
        let taddr = derive_address_offline(POOL_MNEMONIC, "transparent", 0).unwrap();
        let ua = derive_address_offline(POOL_MNEMONIC, "orchard", 0).unwrap();
        let zebrad = Zebrad::start_with_miner(node_bin, &taddr).await.unwrap();
        zebrad.generate_blocks(SEED_BLOCKS).await.unwrap();
        let mut zecd_cfg = ZecdConfig::new(zebrad.rpc_port, pick_port().unwrap());
        zecd_cfg.transparent = true;
        zecd_cfg.restore_mnemonic = Some(POOL_MNEMONIC.to_string());
        zecd_cfg.birthday = Some(BIRTHDAY);
        let zecd = Zecd::start(&zecd_cfg).await.unwrap();
        let pool = Pool {
            zebrad,
            zecd,
            zecd_cfg,
            taddr,
            ua,
            dir: tempfile::tempdir().unwrap(),
        };
        pool.mine(1).await;
        pool.wait_ready().await;
        pool
    }

    async fn wait_ready(&self) {
        let deadline = Instant::now() + SYNC_TIMEOUT;
        loop {
            let peers = self.zecd.call("getpeerinfo", json!([])).await.unwrap();
            if peers
                .as_array()
                .and_then(|a| a.first())
                .is_some_and(|p| p["conn_state"] == "ready")
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "zecd never became ready: {peers}"
            );
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    pub async fn mine(&self, n: u32) {
        // Large requests in chunks keep each `generate` call well under its RPC timeout.
        let mut left = n;
        while left > 0 {
            let k = left.min(50);
            self.zebrad.generate_blocks(k).await.unwrap();
            left -= k;
        }
        self.sync().await;
    }

    pub async fn sync(&self) {
        self.zecd
            .wait_until_synced_to_node(&self.zebrad, SYNC_TIMEOUT)
            .await
            .unwrap();
    }

    pub async fn call(&self, method: &str, params: Value) -> Value {
        self.zecd
            .call(method, params)
            .await
            .unwrap_or_else(|e| panic!("{method}: {e}"))
    }

    pub async fn tip(&self) -> u64 {
        self.zebrad
            .rpc("getblockcount", json!([]))
            .await
            .unwrap()
            .as_u64()
            .unwrap()
    }

    /// Mature coinbase outpoints at the pool's t-address.
    pub async fn mature_coinbase(&self) -> BTreeSet<(String, u64)> {
        let v = self
            .call("listunspent", json!([100, 9_999_999, [self.taddr]]))
            .await;
        v.as_array()
            .unwrap()
            .iter()
            .filter(|u| u["generated"] == json!(true))
            .map(|u| {
                (
                    u["txid"].as_str().unwrap().to_owned(),
                    u["vout"].as_u64().unwrap(),
                )
            })
            .collect()
    }

    pub fn state_dir(&self) -> PathBuf {
        self.dir.path().join("state")
    }

    /// Writes a config file for the tool. `body` holds the [sweep] and/or [refill] sections.
    pub fn config(&self, name: &str, shielded_address: &str, general: &str, body: &str) -> PathBuf {
        let text = format!(
            r#"shielded_address = "{shielded_address}"
[rpc]
url = "http://127.0.0.1:{}/"
user = "{}"
timeout_secs = 10
submit_timeout_secs = 120
read_retries = 3
[general]
state_dir = "{}"
interval_secs = 2
max_backoff_secs = 8
max_tip_lag_blocks = 2
skipped_cycles_before_error = 3
operation_timeout_secs = 300
confirmation_timeout_secs = 300
{general}
{body}
"#,
            self.zecd_cfg.rpc_port,
            self.zecd_cfg.rpc_user,
            self.state_dir().display()
        );
        let path = self.dir.path().join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    /// Starts the tool with its log in `<dir>/<log>`.
    pub fn spawn_tool(&self, args: &[&str], log: &str) -> Tool {
        let log_path = self.dir.path().join(log);
        let file = std::fs::File::create(&log_path).unwrap();
        let child = std::process::Command::new(env!("CARGO_BIN_EXE_coinbase-sweeper"))
            .args(args)
            .env("SWEEPER_RPC_PASSWORD", &self.zecd_cfg.rpc_password)
            .env("RUST_LOG", "info")
            .stdout(Stdio::null())
            .stderr(file)
            .spawn()
            .unwrap();
        Tool { child, log_path }
    }

    /// Runs the tool to completion while mining one block each time it starts waiting for a
    /// confirmation. Returns the exit code and the log.
    pub async fn run_tool_mining(&self, args: &[&str], log: &str) -> (i32, String) {
        let mut tool = self.spawn_tool(args, log);
        let mut mined_for = 0;
        let deadline = Instant::now() + Duration::from_secs(900);
        loop {
            if let Some(status) = tool.child.try_wait().unwrap() {
                return (status.code().unwrap_or(-1), tool.log());
            }
            let waits = tool.log().matches("event=wait_confirmation").count();
            if waits > mined_for {
                self.mine(1).await;
                mined_for = waits;
            }
            assert!(
                Instant::now() < deadline,
                "the tool did not finish: {}",
                tool.log()
            );
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    /// The transaction, verbose, from the wallet.
    pub async fn tx(&self, txid: &str) -> Value {
        self.call("getrawtransaction", json!([txid, 1])).await
    }
}

pub struct Tool {
    pub child: std::process::Child,
    pub log_path: PathBuf,
}

impl Tool {
    pub fn log(&self) -> String {
        std::fs::read_to_string(&self.log_path).unwrap_or_default()
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    pub async fn wait_for_log(&self, needle: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while !self.log().contains(needle) {
            assert!(
                Instant::now() < deadline,
                "log never showed {needle:?}: {}",
                self.log()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for Tool {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Every `key=value` value for `key` in the log's lines that contain `event`.
pub fn log_values(log: &str, event: &str, key: &str) -> Vec<String> {
    let prefix = format!("{key}=");
    log.lines()
        .filter(|l| l.contains(event))
        .filter_map(|l| {
            l.split_whitespace()
                .find_map(|w| w.strip_prefix(&prefix).map(str::to_owned))
        })
        .collect()
}

/// The transparent outpoints a transaction spends.
pub fn inputs(tx: &Value) -> Vec<(String, u64)> {
    tx["vin"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            (
                i["txid"].as_str().unwrap().to_owned(),
                i["vout"].as_u64().unwrap(),
            )
        })
        .collect()
}

pub fn sweep_section(taddr: &str, extra: &str) -> String {
    format!(
        "[sweep]\ncoinbase_addresses = [\"{taddr}\"]\nmin_inputs = 1\nmax_wait_blocks = 0\n\
         alert_unswept_zec = 100000\n{extra}\n"
    )
}
