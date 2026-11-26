//! Settling a refill that was interrupted between recording it and seeing its result.
//!
//! Evidence, strongest first:
//! 1. the operation, by its id when the tool got one, or else a `z_sendmany` that appeared after
//!    the record was written, from the pool's address, paying the same amount to the same
//!    destination;
//! 2. a wallet transaction paying that amount to the destination after the record was written;
//! 3. for the "not sent" verdict: the wallet has not restarted since the record (so a submitted
//!    operation would still be listed), and the destination is one the wallet owns (so a
//!    transfer to it would show in its history).
//!
//! Anything weaker is `Unknown`, and the caller refuses to send.

use std::time::Duration;

use serde_json::{json, Value};

use crate::amount::Zat;
use crate::runner::{Ctx, CycleError};
use crate::wallet::{OpState, Operation};

use super::InFlight;

#[derive(Debug, PartialEq, Eq)]
pub enum Settled {
    Sent(String),
    NotSent(String),
    /// Too soon to conclude "not sent": a request whose answer was lost may still be in the
    /// wallet's queue. Send nothing and look again later.
    Wait(String),
    Unknown(String),
}

/// How old an in-flight record must be before missing evidence can mean "not sent".
pub fn settle_delay(submit_timeout: Duration) -> u64 {
    (submit_timeout.as_secs() * 2).max(600)
}

pub fn settle(ctx: &Ctx, f: &InFlight, timeout: Duration) -> Result<Settled, CycleError> {
    let age = now_since(f.started_unix);
    let wallet = ctx.wallet();
    let ops = wallet.settled_operations(timeout)?;
    let found = match &f.opid {
        Some(id) => ops.iter().find(|o| &o.id == id),
        None => ops
            .iter()
            .find(|o| !f.ops_before.contains(&o.id) && is_own_send(ctx, o, f)),
    };
    if let Some(op) = found {
        wallet.clear_operation(&op.id);
        return Ok(match &op.state {
            OpState::Success { txid } => Settled::Sent(txid.clone()),
            OpState::Failed { code, message } => {
                Settled::NotSent(format!("operation {} failed ({code}): {message}", op.id))
            }
            other => Settled::NotSent(format!("operation {} ended as {other:?}", op.id)),
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
        return Ok(Settled::Sent(txid));
    }

    let delay = settle_delay(ctx.cfg.submit_timeout());
    if age < delay {
        return Ok(Settled::Wait(format!(
            "no operation or wallet transaction yet; the record is {age}s old and is settled \
             no sooner than {delay}s"
        )));
    }
    let still_running = wallet_uptime(ctx)?
        .zip(f.wallet_uptime)
        .is_some_and(|(now, then)| now >= then + now_since(f.started_unix).saturating_sub(5));
    let owned_destination = ctx
        .wallet()
        .is_mine(&f.destination, !looks_transparent(ctx, &f.destination)?)?;
    if still_running && history.is_some() && owned_destination {
        return Ok(Settled::NotSent(
            "the wallet has not restarted, lists no such operation, and its history shows no \
             such transfer"
                .into(),
        ));
    }
    Ok(Settled::Unknown(format!(
        "no matching operation or wallet transaction (wallet_running_since_record={still_running}, \
         history_available={}, destination_owned={owned_destination})",
        history.is_some()
    )))
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
/// record (with a minute of clock slack).
pub fn matching_transfer(history: &Value, f: &InFlight) -> Option<String> {
    let since = f.started_unix.saturating_sub(60);
    history.as_array()?.iter().find_map(|e| {
        let to_dest = e.get("address").and_then(Value::as_str) == Some(f.destination.as_str());
        let amount = e.get("amount").and_then(|a| match a {
            Value::Number(n) => Zat::parse_zec(n.to_string().trim_start_matches('-')).ok(),
            _ => None,
        });
        let time = e
            .get("timereceived")
            .or_else(|| e.get("time"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        (to_dest && amount == Some(f.amount) && time >= since)
            .then(|| e.get("txid").and_then(Value::as_str).map(str::to_owned))
            .flatten()
    })
}

pub fn wallet_uptime(ctx: &Ctx) -> Result<Option<u64>, CycleError> {
    match ctx.rpc.read("uptime", json!([])) {
        Ok(v) => Ok(v.as_u64()),
        Err(e) if e.is_transient() => Err(e.into()),
        Err(_) => Ok(None),
    }
}

fn now_since(unix: u64) -> u64 {
    crate::state::now_unix().saturating_sub(unix)
}

fn looks_transparent(ctx: &Ctx, address: &str) -> Result<bool, CycleError> {
    let v = ctx.rpc.read("validateaddress", json!([address]))?;
    Ok(v.get("isvalid").and_then(Value::as_bool) == Some(true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn f() -> InFlight {
        InFlight {
            started_unix: 1000,
            day: 0,
            amount: Zat(150_000_000),
            destination: "tmDest".into(),
            opid: None,
            ops_before: BTreeSet::new(),
            wallet_uptime: Some(50),
        }
    }

    fn history(entries: Value) -> Value {
        serde_json::from_str(&entries.to_string()).unwrap()
    }

    #[test]
    fn finds_the_matching_transfer() {
        let h = history(json!([
            {"address": "tmDest", "amount": 1.5, "time": 900, "txid": "old"},
            {"address": "tmOther", "amount": 1.5, "time": 1010, "txid": "other"},
            {"address": "tmDest", "amount": 1.4, "time": 1010, "txid": "smaller"},
            {"address": "tmDest", "amount": -1.5, "time": 1005, "txid": "ours"}
        ]));
        assert_eq!(matching_transfer(&h, &f()).as_deref(), Some("ours"));
        let none = history(json!([{"address": "tmDest", "amount": 1.5, "time": 100, "txid": "x"}]));
        assert_eq!(matching_transfer(&none, &f()), None);
    }
}
