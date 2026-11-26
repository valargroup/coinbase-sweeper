//! Settling a refill that was interrupted between recording it and seeing its result, or whose
//! operation failed in a way that may still have left a transaction behind.
//!
//! "Sent" needs the transaction: from the operation, by its id or as a new `z_sendmany` from
//! the pool's address paying the same amount to the same destination, or from a wallet history
//! entry paying that amount to the destination after the record was written.
//!
//! "Not sent" needs proof that no funds moved: every note the source held just before the send
//! is still unspent. A wallet that cannot list notes never gets a "not sent" verdict. Without an
//! operation that finished, the proof is also only accepted once the record is older than the
//! settle delay, because a request whose answer was lost may still be queued in the wallet.
//!
//! Anything else is `Unknown`, and the caller refuses to send.

use std::collections::BTreeSet;
use std::time::Duration;

use serde_json::{json, Value};

use crate::amount::Zat;
use crate::rpc::{RpcError, RPC_WALLET_INSUFFICIENT_FUNDS};
use crate::runner::{Ctx, CycleError};
use crate::state::now_unix;
use crate::wallet::{OpState, Operation};

use super::InFlight;

#[derive(Debug, PartialEq, Eq)]
pub enum Settled {
    /// `opid`: the wallet operation to clear once the outcome is saved.
    Sent {
        txid: String,
        opid: Option<String>,
    },
    NotSent {
        why: String,
        opid: Option<String>,
    },
    /// Too soon to conclude: send nothing and look again later.
    Wait(String),
    Unknown(String),
}

/// How old an in-flight record must be before missing evidence can mean "not sent".
pub fn settle_delay(submit_timeout: Duration) -> u64 {
    (submit_timeout.as_secs() * 2).max(600)
}

pub fn settle(ctx: &Ctx, f: &InFlight, timeout: Duration) -> Result<Settled, CycleError> {
    let wallet = ctx.wallet();
    // With the operation id in hand, ask for that operation directly; otherwise look for a new
    // `z_sendmany` of ours among everything the wallet still lists.
    let found: Option<(String, OpState)> = match &f.opid {
        Some(id) => wallet
            .wait_operation(id, timeout, false)?
            .map(|state| (id.clone(), state)),
        None => wallet
            .settled_operations(timeout)?
            .into_iter()
            .find(|o| !f.ops_before.contains(&o.id) && is_own_send(ctx, o, f))
            .map(|o| (o.id, o.state)),
    };
    if let Some((id, state)) = found {
        let op = Operation {
            id,
            state,
            method: None,
            params: Value::Null,
        };
        let opid = Some(op.id.clone());
        return Ok(match &op.state {
            OpState::Success { txid } => Settled::Sent {
                txid: txid.clone(),
                opid,
            },
            // The wallet could not fund it, so it built no transaction.
            OpState::Failed { code, message } if *code == RPC_WALLET_INSUFFICIENT_FUNDS => {
                Settled::NotSent {
                    why: format!("operation {} found no funds: {message}", op.id),
                    opid,
                }
            }
            other => match notes_unspent(ctx, f)? {
                Some(true) => Settled::NotSent {
                    why: format!(
                        "operation {} ended as {other:?} and no source note was spent",
                        op.id
                    ),
                    opid,
                },
                Some(false) => Settled::Unknown(format!(
                    "operation {} ended as {other:?}, but source notes were spent: the wallet \
                     may have kept the transaction",
                    op.id
                )),
                None => Settled::Unknown(format!(
                    "operation {} ended as {other:?} and the wallet cannot list notes to show \
                     nothing was spent",
                    op.id
                )),
            },
        });
    }

    let history = match ctx
        .rpc
        .read("listtransactions", json!(["*", 1000, 0, true]))
    {
        Ok(v) => Some(v),
        Err(e) if e.is_transient() => return Err(e.into()),
        Err(e) => {
            log::warn!("event=recover_history_unavailable cmd=refill err={e}");
            None
        }
    };
    if let Some(txid) = history.as_ref().and_then(|h| matching_transfer(h, f)) {
        return Ok(Settled::Sent { txid, opid: None });
    }

    let age = now_unix().saturating_sub(f.started_unix);
    let delay = settle_delay(ctx.cfg.submit_timeout());
    if age < delay {
        return Ok(Settled::Wait(format!(
            "no operation or wallet transaction yet; the record is {age}s old and is settled \
             no sooner than {delay}s"
        )));
    }
    Ok(match notes_unspent(ctx, f)? {
        Some(true) => Settled::NotSent {
            why: "no operation, no wallet transaction, and every source note from before the \
                  send is still unspent"
                .into(),
            opid: None,
        },
        Some(false) => Settled::Unknown(
            "source notes were spent, but no operation or wallet transaction matches the \
             transfer"
                .into(),
        ),
        None => Settled::Unknown(
            "no matching operation or wallet transaction, and the wallet cannot list notes".into(),
        ),
    })
}

/// The pool address's unspent notes, as `txid:pool:index`, including unconfirmed ones.
/// zcashd lists them with `z_listunspent`; zecd lists them in `listunspent`. `None` when the
/// wallet supports neither.
pub fn source_notes(ctx: &Ctx) -> Result<Option<BTreeSet<String>>, CycleError> {
    let ua = &ctx.cfg.shielded_address;
    let key = |e: &Value, index_field: &str| -> Option<String> {
        Some(format!(
            "{}:{}:{}",
            e.get("txid")?.as_str()?,
            e.get("pool")?.as_str()?,
            e.get(index_field)?.as_u64()?
        ))
    };
    let malformed = |method: &str| {
        CycleError::Failed(anyhow::anyhow!(
            "{method} returned a note without txid, pool or index"
        ))
    };
    match ctx
        .rpc
        .read("z_listunspent", json!([0, 9_999_999, false, [ua]]))
    {
        Ok(v) => {
            let notes = v
                .as_array()
                .ok_or_else(|| RpcError::Protocol(format!("z_listunspent returned {v}")))?;
            return notes
                .iter()
                .map(|e| key(e, "outindex"))
                .collect::<Option<BTreeSet<String>>>()
                .map(Some)
                .ok_or_else(|| malformed("z_listunspent"));
        }
        Err(e) if e.is_transient() => return Err(e.into()),
        Err(_) => {}
    }
    match ctx.rpc.read("listunspent", json!([0, 9_999_999])) {
        Ok(v) => {
            let entries = v
                .as_array()
                .ok_or_else(|| RpcError::Protocol(format!("listunspent returned {v}")))?;
            entries
                .iter()
                .filter(|e| {
                    matches!(
                        e.get("pool").and_then(Value::as_str),
                        Some("orchard" | "ironwood")
                    )
                })
                .map(|e| key(e, "vout"))
                .collect::<Option<BTreeSet<String>>>()
                .map(Some)
                .ok_or_else(|| malformed("listunspent"))
        }
        Err(e) if e.is_transient() => Err(e.into()),
        Err(_) => Ok(None),
    }
}

/// `Some(true)` when every note recorded before the send is still unspent.
fn notes_unspent(ctx: &Ctx, f: &InFlight) -> Result<Option<bool>, CycleError> {
    let Some(before) = &f.notes_before else {
        return Ok(None);
    };
    let Some(now) = source_notes(ctx)? else {
        return Ok(None);
    };
    Ok(Some(before.is_subset(&now)))
}

/// A `z_sendmany` this tool would have started for `f`.
fn is_own_send(ctx: &Ctx, op: &Operation, f: &InFlight) -> bool {
    if op.method.as_deref() != Some("z_sendmany") {
        return false;
    }
    if op.params.get("fromaddress").and_then(Value::as_str) != Some(&ctx.cfg.shielded_address) {
        return false;
    }
    op.params
        .get("amounts")
        .and_then(Value::as_array)
        .is_some_and(|outs| {
            outs.iter().any(|o| {
                o.get("address").and_then(Value::as_str) == Some(f.destination.as_str())
                    && o.get("amount").and_then(|a| Zat::from_json(a).ok()) == Some(f.amount)
            })
        })
}

/// A wallet history entry paying exactly `f.amount` to `f.destination`, no earlier than the
/// record (with a few seconds of clock slack), and not the previous refill.
pub fn matching_transfer(history: &Value, f: &InFlight) -> Option<String> {
    let since = f.started_unix.saturating_sub(5);
    history.as_array()?.iter().find_map(|e| {
        let txid = e.get("txid").and_then(Value::as_str)?;
        if f.previous_txid.as_deref() == Some(txid) {
            return None;
        }
        let to_dest = e.get("address").and_then(Value::as_str) == Some(f.destination.as_str());
        let amount = match e.get("amount") {
            Some(Value::Number(n)) => {
                serde_json::from_str::<Value>(n.to_string().trim_start_matches('-'))
                    .ok()
                    .and_then(|abs| Zat::from_json(&abs).ok())
            }
            _ => None,
        };
        let time = e
            .get("timereceived")
            .or_else(|| e.get("time"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        (to_dest && amount == Some(f.amount) && time >= since).then(|| txid.to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f() -> InFlight {
        InFlight {
            started_unix: 1000,
            day: 0,
            amount: Zat(150_000_000),
            destination: "tmDest".into(),
            opid: None,
            ops_before: BTreeSet::new(),
            notes_before: None,
            previous_txid: Some("previous".into()),
        }
    }

    fn history(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn finds_the_matching_transfer() {
        let h = history(
            r#"[
            {"address": "tmDest", "amount": 1.5, "time": 900, "txid": "old"},
            {"address": "tmDest", "amount": 1.5, "time": 1001, "txid": "previous"},
            {"address": "tmOther", "amount": 1.5, "time": 1010, "txid": "other"},
            {"address": "tmDest", "amount": 1.4, "time": 1010, "txid": "smaller"},
            {"address": "tmDest", "amount": -1.5, "time": 1005, "txid": "ours"}
        ]"#,
        );
        assert_eq!(matching_transfer(&h, &f()).as_deref(), Some("ours"));
        let none = history(r#"[{"address": "tmDest", "amount": 1.5, "time": 100, "txid": "x"}]"#);
        assert_eq!(matching_transfer(&none, &f()), None);
        let sci = history(r#"[{"address": "tmDest", "amount": 1.5e0, "time": 1003, "txid": "s"}]"#);
        assert_eq!(matching_transfer(&sci, &f()).as_deref(), Some("s"));
    }
}
