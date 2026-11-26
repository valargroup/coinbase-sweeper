//! Typed wrappers over the wallet RPC calls the tool uses. Every response is parsed strictly:
//! a field the tool depends on that is missing or malformed is an error, never a default.

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::amount::Zat;
use crate::rpc::{Rpc, RpcError};

/// Blocks past a transaction's expiry height before the tool treats it as never confirming.
pub const EXPIRY_MARGIN: u64 = 3;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxStatus {
    Confirmed(u64),
    /// In the mempool (or not yet relayed).
    Pending,
    /// Unknown to the wallet, conflicted or expired: it will never confirm.
    Gone,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpState {
    Queued,
    Executing,
    Success {
        txid: String,
    },
    Failed {
        code: i64,
        message: String,
    },
    Cancelled,
    /// Finished, in a shape the tool does not use.
    Other(String),
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

    /// Polls one operation until it finishes. With `clear`, the finished result is then removed
    /// from the wallet's memory; only the tool's own operations are cleared, so other software
    /// polling the same wallet (the pool's payout code) still finds its results.
    /// `Ok(None)` means the wallet no longer knows the operation (it restarted).
    pub fn wait_operation(
        &self,
        opid: &str,
        timeout: Duration,
        clear: bool,
    ) -> Result<Option<OpState>> {
        let deadline = Instant::now() + timeout;
        let mut delay = Duration::from_millis(200);
        loop {
            let ops = self.operations(&[opid.to_owned()])?;
            match ops.into_iter().find(|o| o.id == opid) {
                None => return Ok(None),
                Some(op) if op.state.is_finished() => {
                    if clear {
                        self.clear_operation(opid);
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

    /// Removes a finished operation's result. Best effort: an uncleared result only costs the
    /// wallet a little memory.
    pub fn clear_operation(&self, opid: &str) {
        if let Err(e) = self.rpc.read("z_getoperationresult", json!([[opid]])) {
            log::warn!("event=op_clear_failed opid={opid} err={e}");
        }
    }

    /// Waits until no operation is queued or executing, then returns every operation the wallet
    /// still remembers, all finished. Results are not cleared.
    pub fn settled_operations(&self, timeout: Duration) -> Result<Vec<Operation>> {
        let ids = self.operation_ids()?;
        let mut ops = self.operations(&ids)?;
        for op in ops.iter_mut().filter(|o| !o.state.is_finished()) {
            log::info!(
                "event=wait_pending_op opid={} method={}",
                op.id,
                op.method.as_deref().unwrap_or("?")
            );
            match self.wait_operation(&op.id, timeout, false)? {
                Some(state) => op.state = state,
                // Forgotten by a wallet restart; nothing left to wait for.
                None => op.state = OpState::Cancelled,
            }
        }
        Ok(ops)
    }

    pub fn raw_transaction(&self, txid: &str) -> Result<Value, RpcError> {
        self.rpc.read("getrawtransaction", json!([txid, 1]))
    }

    /// The verbose transaction. Falls back to decoding the wallet's own copy, for a node that has
    /// no transaction index once the transaction is mined.
    pub fn transaction(&self, txid: &str) -> Result<Value, RpcError> {
        match self.raw_transaction(txid) {
            Err(RpcError::Rpc { code: -5, .. }) => {
                let wtx = self.rpc.read("gettransaction", json!([txid]))?;
                let hex = wtx.get("hex").and_then(Value::as_str).ok_or_else(|| {
                    RpcError::Protocol(format!("gettransaction {txid} has no hex"))
                })?;
                self.rpc.read("decoderawtransaction", json!([hex]))
            }
            other => other,
        }
    }

    /// Where a wallet transaction stands. `tip` is the wallet's height.
    ///
    /// `Gone` is decided conservatively, because it tells the caller the inputs are free again:
    /// only when neither the wallet nor the node knows the transaction, or when it is still
    /// unmined `EXPIRY_MARGIN` blocks after its expiry height. Wallets briefly report 0 or -1
    /// confirmations, or a non-"mined" status, while they process a new block; those readings
    /// are `Pending`, never `Gone`.
    pub fn tx_status(&self, txid: &str, tip: u64) -> Result<TxStatus, RpcError> {
        let v = match self.rpc.read("gettransaction", json!([txid])) {
            Err(RpcError::Rpc { code: -5, .. }) => {
                return match self.raw_transaction(txid) {
                    Ok(tx) => Ok(match tx.get("confirmations").and_then(Value::as_u64) {
                        Some(c) if c > 0 => TxStatus::Confirmed(c),
                        _ => TxStatus::Pending,
                    }),
                    Err(RpcError::Rpc { code: -5, .. }) => Ok(TxStatus::Gone),
                    Err(e) => Err(e),
                };
            }
            other => other?,
        };
        let confirmations = v.get("confirmations").and_then(Value::as_i64).unwrap_or(0);
        if confirmations > 0 {
            return Ok(TxStatus::Confirmed(confirmations as u64));
        }
        let expired = v
            .get("expiryheight")
            .and_then(Value::as_u64)
            .is_some_and(|h| h > 0 && tip > h.saturating_add(EXPIRY_MARGIN));
        Ok(if expired {
            TxStatus::Gone
        } else {
            TxStatus::Pending
        })
    }

    /// Waits until `txid` has `want` confirmations, or returns `TxStatus::Gone` early when it can
    /// no longer confirm.
    pub fn wait_confirmations(&self, txid: &str, want: u64, timeout: Duration) -> Result<TxStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            let tip = self.chain_status().map(|c| c.wallet_height).unwrap_or(0);
            match self.tx_status(txid, tip) {
                Ok(TxStatus::Confirmed(c)) if c >= want => return Ok(TxStatus::Confirmed(c)),
                Ok(TxStatus::Gone) => return Ok(TxStatus::Gone),
                Ok(_) => {}
                Err(e) if e.is_transient() => log::warn!("event=confirm_poll_failed err={e}"),
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
    let node_height = u64_field(v, "headers")?.max(wallet_height);
    // zecd keeps `initialblockdownload` true while it backfills details of transactions it has
    // already scanned; that does not affect balances. Syncing means the flag plus a block scan
    // or verification that is not yet complete.
    let progress = v
        .get("verificationprogress")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    // zecd and bitcoind say `initialblockdownload`; zcashd says `initial_block_download_complete`.
    let in_ibd = v
        .get("initialblockdownload")
        .and_then(Value::as_bool)
        .or_else(|| {
            v.get("initial_block_download_complete")
                .and_then(Value::as_bool)
                .map(|done| !done)
        })
        .unwrap_or(false);
    let initial_block_download = in_ibd && progress < 0.999;
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
                "success" => match o.pointer("/result/txid").and_then(Value::as_str) {
                    Some(txid) => OpState::Success {
                        txid: txid.to_owned(),
                    },
                    // Another program's operation may report its result differently; it is
                    // finished, and for the tool's own operations this is a failure.
                    None => OpState::Other("success without a txid".into()),
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
                other => OpState::Other(format!("unknown status {other:?}")),
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
        let odd = parse_operations(&json!([{"id": "x", "status": "success"},
                                           {"id": "y", "status": "weird"}]))
        .unwrap();
        assert!(matches!(odd[0].state, OpState::Other(_)) && odd[1].state.is_finished());
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
        assert!(
            parse_chain_status(&json!({"blocks": 12})).is_err(),
            "fails closed"
        );
        let ibd = |p: f64| {
            parse_chain_status(
                &json!({"blocks": 1, "headers": 1, "initialblockdownload": true,
                                       "verificationprogress": p}),
            )
            .unwrap()
            .initial_block_download
        };
        assert!(ibd(0.5));
        assert!(!ibd(1.0), "fully scanned, only backfilling details");
        let zcashd = parse_chain_status(&json!({"blocks": 1, "headers": 1,
            "initial_block_download_complete": false, "verificationprogress": 0.2}))
        .unwrap();
        assert!(zcashd.initial_block_download);
    }
}
