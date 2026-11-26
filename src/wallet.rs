//! Typed wrappers over the wallet RPC calls the tool uses. Every response is parsed strictly:
//! a field the tool depends on that is missing or malformed is an error, never a default.

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::amount::Zat;
use crate::rpc::{Rpc, RpcError};

/// Consensus coinbase maturity: a coinbase output is spendable once it has this many
/// confirmations. A fixed rule of the network, not a setting.
pub const COINBASE_MATURITY: u64 = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainStatus {
    /// The height the wallet has fully scanned.
    pub wallet_height: u64,
    /// The best height the wallet knows the node has.
    pub node_height: u64,
    pub initial_block_download: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Utxo {
    pub txid: String,
    pub vout: u64,
    pub address: String,
    pub amount: Zat,
    pub confirmations: u64,
    pub generated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpState {
    Queued,
    Executing,
    Success { txid: String },
    Failed { code: i64, message: String },
    Cancelled,
}

impl OpState {
    pub fn is_finished(&self) -> bool {
        !matches!(self, OpState::Queued | OpState::Executing)
    }
}

#[derive(Clone, Debug)]
pub struct Operation {
    pub id: String,
    pub state: OpState,
    pub method: Option<String>,
    pub params: Value,
}

pub struct Wallet<'a> {
    pub rpc: &'a Rpc,
}

fn field<'v>(v: &'v Value, key: &str) -> Result<&'v Value> {
    v.get(key)
        .with_context(|| format!("missing field {key:?} in {v}"))
}

fn u64_field(v: &Value, key: &str) -> Result<u64> {
    field(v, key)?
        .as_u64()
        .with_context(|| format!("field {key:?} is not a non-negative integer in {v}"))
}

fn str_field<'v>(v: &'v Value, key: &str) -> Result<&'v str> {
    field(v, key)?
        .as_str()
        .with_context(|| format!("field {key:?} is not a string in {v}"))
}

impl<'a> Wallet<'a> {
    pub fn new(rpc: &'a Rpc) -> Self {
        Wallet { rpc }
    }

    pub fn chain_status(&self) -> Result<ChainStatus, RpcError> {
        let v = self.rpc.read("getblockchaininfo", json!([]))?;
        parse_chain_status(&v).map_err(|e| RpcError::Protocol(format!("getblockchaininfo: {e:#}")))
    }

    /// Unspent transparent outputs of `addresses` with at least `minconf` confirmations.
    pub fn list_unspent(&self, minconf: u64, addresses: &[String]) -> Result<Vec<Utxo>, RpcError> {
        let v = self
            .rpc
            .read("listunspent", json!([minconf, 9_999_999, addresses]))?;
        parse_unspent(&v).map_err(|e| RpcError::Protocol(format!("listunspent: {e:#}")))
    }

    pub fn operation_ids(&self) -> Result<Vec<String>, RpcError> {
        let v = self.rpc.read("z_listoperationids", json!([]))?;
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_owned))
                    .collect()
            })
            .ok_or_else(|| RpcError::Protocol(format!("z_listoperationids: {v}")))
    }

    pub fn operations(&self, ids: &[String]) -> Result<Vec<Operation>, RpcError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let v = self.rpc.read("z_getoperationstatus", json!([ids]))?;
        parse_operations(&v).map_err(|e| RpcError::Protocol(format!("z_getoperationstatus: {e:#}")))
    }

    /// Polls one operation until it finishes, then clears it from the wallet's memory.
    /// `Ok(None)` means the wallet no longer knows the operation (it restarted).
    pub fn wait_operation(&self, opid: &str, timeout: Duration) -> Result<Option<OpState>> {
        let deadline = Instant::now() + timeout;
        let mut delay = Duration::from_millis(200);
        loop {
            let ops = self.operations(&[opid.to_owned()])?;
            match ops.into_iter().find(|o| o.id == opid) {
                None => return Ok(None),
                Some(op) if op.state.is_finished() => {
                    // Best effort: an uncleared result only costs the wallet a little memory.
                    if let Err(e) = self.rpc.read("z_getoperationresult", json!([[opid]])) {
                        log::warn!("event=op_clear_failed opid={opid} err={e}");
                    }
                    return Ok(Some(op.state));
                }
                Some(_) => {}
            }
            if Instant::now() >= deadline {
                bail!("operation {opid} did not finish within {timeout:?}");
            }
            std::thread::sleep(delay);
            delay = (delay * 2).min(Duration::from_secs(2));
        }
    }

    /// Waits for every queued or executing operation the wallet knows about.
    pub fn wait_all_pending(&self, timeout: Duration) -> Result<usize> {
        let ids = self.operation_ids()?;
        let pending: Vec<Operation> = self
            .operations(&ids)?
            .into_iter()
            .filter(|o| !o.state.is_finished())
            .collect();
        for op in &pending {
            log::info!(
                "event=wait_pending_op opid={} method={}",
                op.id,
                op.method.as_deref().unwrap_or("?")
            );
            self.wait_operation(&op.id, timeout)?;
        }
        Ok(pending.len())
    }

    pub fn raw_transaction(&self, txid: &str) -> Result<Value, RpcError> {
        self.rpc.read("getrawtransaction", json!([txid, 1]))
    }

    /// Confirmations of a wallet transaction; 0 while it is in the mempool.
    pub fn confirmations(&self, txid: &str) -> Result<u64, RpcError> {
        let v = self.rpc.read("gettransaction", json!([txid]))?;
        // A conflicted transaction reports -1; treat it as unconfirmed.
        Ok(v.get("confirmations")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            .max(0) as u64)
    }

    pub fn wait_confirmations(&self, txid: &str, want: u64, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.confirmations(txid) {
                Ok(c) if c >= want => return Ok(()),
                Ok(_) => {}
                Err(e) if e.is_connectivity() => log::warn!("event=confirm_poll_failed err={e}"),
                Err(e) => return Err(anyhow!(e).context(format!("gettransaction {txid}"))),
            }
            if Instant::now() >= deadline {
                bail!("transaction {txid} did not reach {want} confirmation(s) within {timeout:?}");
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    /// `z_validateaddress` / `validateaddress` ownership.
    pub fn is_mine(&self, address: &str, shielded: bool) -> Result<bool, RpcError> {
        let method = if shielded {
            "z_validateaddress"
        } else {
            "validateaddress"
        };
        let v = self.rpc.read(method, json!([address]))?;
        if v.get("isvalid").and_then(Value::as_bool) != Some(true) {
            return Err(RpcError::Rpc {
                code: -5,
                message: format!("{address} is not a valid address for this network"),
            });
        }
        Ok(v.get("ismine").and_then(Value::as_bool).unwrap_or(false))
    }

    /// The receiver types of a unified address (`z_listunifiedreceivers`).
    pub fn unified_receivers(&self, ua: &str) -> Result<Vec<String>, RpcError> {
        let v = self.rpc.read("z_listunifiedreceivers", json!([ua]))?;
        v.as_object()
            .map(|o| o.keys().cloned().collect())
            .ok_or_else(|| RpcError::Protocol(format!("z_listunifiedreceivers: {v}")))
    }

    /// `z_getbalance [address, minconf]`.
    pub fn balance(&self, address: &str, minconf: u32) -> Result<Zat, RpcError> {
        let v = self.rpc.read("z_getbalance", json!([address, minconf]))?;
        Zat::from_json(&v).map_err(|e| RpcError::Protocol(format!("z_getbalance: {e:#}")))
    }
}

pub fn parse_chain_status(v: &Value) -> Result<ChainStatus> {
    let wallet_height = u64_field(v, "blocks")?;
    let node_height = v
        .get("headers")
        .and_then(Value::as_u64)
        .unwrap_or(wallet_height)
        .max(wallet_height);
    let initial_block_download = v
        .get("initialblockdownload")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(ChainStatus {
        wallet_height,
        node_height,
        initial_block_download,
    })
}

pub fn parse_unspent(v: &Value) -> Result<Vec<Utxo>> {
    let arr = v
        .as_array()
        .context("listunspent did not return an array")?;
    let mut out = Vec::with_capacity(arr.len());
    for e in arr {
        // zecd also lists shielded notes here; they carry a non-transparent pool.
        if e.get("pool")
            .and_then(Value::as_str)
            .is_some_and(|p| p != "transparent")
        {
            continue;
        }
        out.push(Utxo {
            txid: str_field(e, "txid")?.to_owned(),
            vout: u64_field(e, "vout")?,
            address: str_field(e, "address")?.to_owned(),
            amount: Zat::from_json(field(e, "amount")?)?,
            confirmations: u64_field(e, "confirmations")?,
            generated: e.get("generated").and_then(Value::as_bool).unwrap_or(false),
        });
    }
    Ok(out)
}

pub fn parse_operations(v: &Value) -> Result<Vec<Operation>> {
    let arr = v.as_array().context("expected an array of operations")?;
    arr.iter()
        .map(|o| {
            let id = str_field(o, "id")?.to_owned();
            let state = match str_field(o, "status")? {
                "queued" => OpState::Queued,
                "executing" => OpState::Executing,
                "success" => OpState::Success {
                    txid: o
                        .pointer("/result/txid")
                        .and_then(Value::as_str)
                        .with_context(|| format!("successful operation {id} has no txid"))?
                        .to_owned(),
                },
                "failed" => OpState::Failed {
                    code: o
                        .pointer("/error/code")
                        .and_then(Value::as_i64)
                        .unwrap_or(0),
                    message: o
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                },
                "cancelled" => OpState::Cancelled,
                other => bail!("operation {id} has unknown status {other:?}"),
            };
            Ok(Operation {
                id,
                state,
                method: o.get("method").and_then(Value::as_str).map(str::to_owned),
                params: o.get("params").cloned().unwrap_or(Value::Null),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_operations() {
        let v = json!([
            {"id": "opid-1", "status": "success", "method": "z_shieldcoinbase",
             "result": {"txid": "ab"}, "params": {}},
            {"id": "opid-2", "status": "failed", "error": {"code": -6, "message": "no funds"}},
            {"id": "opid-3", "status": "executing"}
        ]);
        let ops = parse_operations(&v).unwrap();
        assert_eq!(ops[0].state, OpState::Success { txid: "ab".into() });
        assert_eq!(
            ops[1].state,
            OpState::Failed {
                code: -6,
                message: "no funds".into()
            }
        );
        assert!(!ops[2].state.is_finished());
        assert!(parse_operations(&json!([{"id": "x", "status": "success"}])).is_err());
    }

    #[test]
    fn parses_unspent_and_skips_notes() {
        let v = json!([
            {"txid": "aa", "vout": 0, "address": "t1", "amount": 3.125, "confirmations": 150,
             "generated": true, "pool": "transparent"},
            {"txid": "bb", "vout": 1, "address": "", "amount": 1.0, "confirmations": 5,
             "pool": "orchard"}
        ]);
        let u = parse_unspent(&serde_json::from_str(&v.to_string()).unwrap()).unwrap();
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].amount, Zat(312_500_000));
        assert!(u[0].generated);
        assert!(parse_unspent(&json!([{"txid": "aa"}])).is_err());
    }

    #[test]
    fn chain_status_uses_headers_as_node_tip() {
        let s = parse_chain_status(&json!({"blocks": 10, "headers": 12})).unwrap();
        assert_eq!((s.wallet_height, s.node_height), (10, 12));
        assert!(parse_chain_status(&json!({"headers": 12})).is_err());
    }
}
