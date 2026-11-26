//! `refill`: moves funds from the pool's Orchard-only unified address (its Ironwood notes) to
//! one configured payout destination, within per-transfer and per-day caps.
//!
//! Crash safety rests on the state file. Before every send the tool records the transfer as in
//! flight and adds it to the day's total, then sends. On the next start an in-flight record is
//! settled (sent or not sent) from the wallet before anything else is sent; if it cannot be
//! settled, nothing is sent until an operator resolves it with `--resolve-interrupted`. A crash
//! can therefore only under-send.

pub mod decide;
pub mod recover;

use std::collections::BTreeSet;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::amount::Zat;
use crate::config::{RefillConfig, RefillMode};
use crate::rpc::RPC_WALLET_INSUFFICIENT_FUNDS;
use crate::runner::{is_node_rejection, Ctx, CycleError, Exit, Job, StartupError};
use crate::startup;
use crate::state::now_unix;
use crate::txcheck;
use crate::wallet::{ChainStatus, OpState, TxStatus};

use decide::{decide, utc_day, Decision};
use recover::Settled;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Resolution {
    Sent,
    NotSent,
}

/// A transfer the tool started but has not seen finish.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct InFlight {
    pub started_unix: u64,
    pub day: u64,
    pub amount: Zat,
    pub destination: String,
    /// Set as soon as the wallet answers with an operation id.
    pub opid: Option<String>,
    /// The wallet's operation ids just before sending, to recognise a new one after a crash.
    pub ops_before: BTreeSet<String>,
    /// The wallet's `uptime` just before sending, to tell whether it restarted since.
    #[serde(default)]
    pub wallet_uptime: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RefillState {
    /// The UTC day `sent_today` counts.
    pub day: u64,
    pub sent_today: Zat,
    pub in_flight: Option<InFlight>,
    /// The last transfer, until it has `wait_confirmations` confirmations.
    pub unconfirmed: Option<String>,
}

impl RefillState {
    /// Starts a new day's total when the UTC day has changed.
    pub fn roll_over(&mut self, today: u64) {
        if self.day != today {
            self.day = today;
            self.sent_today = Zat::ZERO;
        }
    }

    /// Takes back an in-flight transfer that did not happen.
    fn refund(&mut self, f: &InFlight) {
        if f.day == self.day {
            self.sent_today = self.sent_today.saturating_sub(f.amount);
        }
    }
}

impl serde::Serialize for Zat {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(self.0)
    }
}

impl<'de> serde::Deserialize<'de> for Zat {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Zat, D::Error> {
        u64::deserialize(d).map(Zat)
    }
}

pub struct Refill {
    cfg: RefillConfig,
    /// Learned at startup: a transparent destination gets a transparent output.
    transparent_destination: bool,
}

impl Refill {
    pub fn new(cfg: RefillConfig) -> Refill {
        Refill {
            cfg,
            transparent_destination: false,
        }
    }

    fn load(&self, ctx: &Ctx) -> Result<RefillState, CycleError> {
        let mut s: RefillState = ctx.state.read_json("state")?.unwrap_or_default();
        s.roll_over(utc_day(now_unix()));
        Ok(s)
    }

    fn save(&self, ctx: &Ctx, s: &RefillState) -> Result<(), CycleError> {
        Ok(ctx.state.write_json("state", s)?)
    }

    fn policy(&self) -> &'static str {
        if self.transparent_destination {
            "AllowRevealedRecipients"
        } else {
            "FullPrivacy"
        }
    }
}

impl Job for Refill {
    fn startup(&mut self, ctx: &Ctx) -> Result<(), StartupError> {
        startup::check_shielded_address(ctx)?;
        let dest = &self.cfg.destination;
        let wallet = ctx.wallet();
        self.transparent_destination = startup::is_transparent(ctx, dest)?;
        let owned = if self.transparent_destination {
            wallet.is_mine(dest, false)?
        } else {
            let receivers = wallet.unified_receivers(dest).map_err(|e| {
                if e.is_transient() {
                    StartupError::from(e)
                } else {
                    StartupError::Refuse(anyhow!(
                        "refill.destination {dest} is neither a transparent address nor a \
                         unified address: {e}"
                    ))
                }
            })?;
            if !receivers.iter().any(|r| r == "orchard") {
                return Err(StartupError::Refuse(anyhow!(
                    "refill.destination {dest} has no Orchard receiver, so a transfer to it \
                     could not stay in Ironwood"
                )));
            }
            wallet.is_mine(dest, true)?
        };
        if !owned && self.cfg.external_destination.as_deref() != Some(dest.as_str()) {
            return Err(StartupError::Refuse(anyhow!(
                "refill.destination {dest} does not belong to this wallet. If it is an external \
                 payout wallet, confirm it by also setting external_destination = \"{dest}\""
            )));
        }
        if self.cfg.mode == RefillMode::TopUp && !(owned && self.transparent_destination) {
            return Err(StartupError::Refuse(anyhow!(
                "top_up mode needs a transparent destination this wallet owns (it reads that \
                 address's balance); use drain_above for any other destination"
            )));
        }
        if !ctx.dry_run {
            wallet
                .settled_operations(op_timeout(ctx))
                .map_err(|e| StartupError::Unavailable(format!("{e:#}")))?;
        }
        log::info!(
            "event=refill_destination cmd=refill addr={dest} owned={owned} transparent={} \
             mode={:?}",
            self.transparent_destination,
            self.cfg.mode
        );
        Ok(())
    }

    fn cycle(&mut self, ctx: &Ctx, chain: &ChainStatus) -> Result<(), CycleError> {
        let mut state = self.load(ctx)?;
        if !ctx.dry_run {
            if state.in_flight.is_some() {
                self.settle_in_flight(ctx, &mut state)?;
            }
            if let Some(txid) = state.unconfirmed.clone() {
                self.wait_unconfirmed(ctx, chain, &mut state, &txid)?;
            }
        }

        let wallet = ctx.wallet();
        let ua = &ctx.cfg.shielded_address;
        let spendable = wallet.balance(ua, self.cfg.minconf)?;
        let dest_balance = match self.cfg.mode {
            RefillMode::TopUp => Some(self.destination_balance(ctx)?),
            RefillMode::DrainAbove => None,
        };
        let decision = decide(&self.cfg, spendable, dest_balance, state.sent_today);
        let amount = match decision {
            Decision::Skip(reason) => {
                log::info!(
                    "event=refill_skip cmd=refill addr={} source_spendable={spendable} \
                     dest_balance={} sent_today={} reason={reason:?}",
                    self.cfg.destination,
                    dest_balance.map_or("n/a".into(), |b| b.to_string()),
                    state.sent_today
                );
                return Ok(());
            }
            Decision::Send(amount) => amount,
        };
        if ctx.dry_run {
            log::info!(
                "event=dry_run cmd=refill addr={} would_send={amount} source_spendable={spendable} \
                 sent_today={} max_per_day={}",
                self.cfg.destination,
                state.sent_today,
                self.cfg.max_per_day
            );
            return Ok(());
        }
        self.send(ctx, &mut state, amount)
    }
}

impl Refill {
    /// The destination's payable balance, including unconfirmed receipts so a transfer still in
    /// the mempool is not topped up twice. Coinbase at the destination is left out: it cannot
    /// pay miners until it is shielded, so it must not make the address look funded.
    fn destination_balance(&self, ctx: &Ctx) -> Result<Zat, CycleError> {
        let utxos = ctx
            .wallet()
            .list_unspent(0, std::slice::from_ref(&self.cfg.destination))?;
        let mut total = Zat::ZERO;
        for u in utxos.into_iter().filter(|u| !u.generated) {
            total = total
                .checked_add(u.amount)
                .context("destination balance overflows")?;
        }
        Ok(total)
    }

    fn send(&self, ctx: &Ctx, state: &mut RefillState, amount: Zat) -> Result<(), CycleError> {
        let dest = self.cfg.destination.clone();
        let wallet = ctx.wallet();
        let ops_before: BTreeSet<String> = wallet.operation_ids()?.into_iter().collect();
        let wallet_uptime = recover::wallet_uptime(ctx)?;
        // Record first, send second: a crash from here on leaves an in-flight record that must
        // be settled before anything else is sent.
        state.sent_today = state
            .sent_today
            .checked_add(amount)
            .context("daily total overflows")?;
        state.in_flight = Some(InFlight {
            started_unix: now_unix(),
            day: state.day,
            amount,
            destination: dest.clone(),
            opid: None,
            ops_before,
            wallet_uptime,
        });
        self.save(ctx, state)?;
        log::info!(
            "event=refill_submitting cmd=refill addr={dest} amount={amount} sent_today={}",
            state.sent_today
        );
        let params = json!([
            ctx.cfg.shielded_address,
            [{"address": dest, "amount": amount.to_json()}],
            self.cfg.minconf,
            Value::Null,
            self.policy()
        ]);
        let opid = match ctx.rpc.submit("z_sendmany", params) {
            Ok(v) => match v.as_str() {
                Some(id) => id.to_owned(),
                None => {
                    log::error!("event=submit_unknown cmd=refill addr={dest} reply={v}");
                    return self.settle_in_flight(ctx, state);
                }
            },
            Err(e) if e.outcome_unknown() => {
                log::error!("event=submit_unknown cmd=refill addr={dest} amount={amount} err={e}");
                return self.settle_in_flight(ctx, state);
            }
            Err(e) => {
                // A definite refusal: nothing was sent.
                self.not_sent(ctx, state)?;
                if e.code() == Some(RPC_WALLET_INSUFFICIENT_FUNDS) {
                    log::warn!(
                        "event=refill_skip cmd=refill addr={dest} amount={amount} \
                         reason=insufficient_funds detail={e:?}"
                    );
                    return Ok(());
                }
                return Err(e.into());
            }
        };
        if let Some(f) = state.in_flight.as_mut() {
            f.opid = Some(opid.clone());
        }
        self.save(ctx, state)?;
        log::info!("event=refill_submitted cmd=refill addr={dest} amount={amount} opid={opid}");
        match wallet.wait_operation(&opid, op_timeout(ctx), true)? {
            Some(OpState::Success { txid }) => self.sent(ctx, state, &txid, "ok"),
            Some(OpState::Failed { code, message }) => {
                self.not_sent(ctx, state)?;
                if is_node_rejection(code, &message) {
                    log::error!(
                        "event=refill_failed cmd=refill addr={dest} amount={amount} opid={opid} \
                         code={code} reason={message:?} outcome=rejected"
                    );
                    return Err(CycleError::Rejected(message));
                }
                if code == RPC_WALLET_INSUFFICIENT_FUNDS {
                    // Not retried with a smaller amount in this cycle.
                    log::warn!(
                        "event=refill_skip cmd=refill addr={dest} amount={amount} opid={opid} \
                         reason=insufficient_funds detail={message:?}"
                    );
                    return Ok(());
                }
                Err(CycleError::Failed(anyhow!(
                    "refill operation {opid} failed ({code}): {message}"
                )))
            }
            Some(other) => {
                self.not_sent(ctx, state)?;
                Err(CycleError::Failed(anyhow!(
                    "refill operation {opid} ended as {other:?}"
                )))
            }
            // The wallet restarted: settle from its history next cycle.
            None => Err(CycleError::Failed(anyhow!(
                "the wallet lost refill operation {opid}; it will be settled before any new send"
            ))),
        }
    }

    /// Settles the in-flight record. Returns an error (and sends nothing) when it cannot be
    /// settled.
    fn settle_in_flight(&self, ctx: &Ctx, state: &mut RefillState) -> Result<(), CycleError> {
        let f = state
            .in_flight
            .clone()
            .expect("called only with an in-flight record");
        match recover::settle(ctx, &f, op_timeout(ctx))? {
            Settled::Sent(txid) => self.sent(ctx, state, &txid, "ok_recovered"),
            Settled::NotSent(why) => {
                log::warn!(
                    "event=refill_not_sent cmd=refill addr={} amount={} reason={why:?}",
                    f.destination,
                    f.amount
                );
                self.not_sent(ctx, state)
            }
            Settled::Wait(why) => {
                log::warn!(
                    "event=refill_settling cmd=refill addr={} amount={} reason={why:?}",
                    f.destination,
                    f.amount
                );
                Err(CycleError::Skip(format!(
                    "an interrupted refill of {} ZEC is not settled yet; sending nothing",
                    f.amount
                )))
            }
            Settled::Unknown(why) => {
                log::error!(
                    "event=refill_unsettled cmd=refill addr={} amount={} started_unix={} \
                     reason={why:?} fix=\"check the wallet and the destination, then run \
                     refill --resolve-interrupted sent|not-sent\"",
                    f.destination,
                    f.amount,
                    f.started_unix
                );
                Err(CycleError::Failed(anyhow!(
                    "an interrupted refill of {} ZEC to {} cannot be settled; refusing to send",
                    f.amount,
                    f.destination
                )))
            }
        }
    }

    fn not_sent(&self, ctx: &Ctx, state: &mut RefillState) -> Result<(), CycleError> {
        if let Some(f) = state.in_flight.take() {
            state.refund(&f);
        }
        self.save(ctx, state)
    }

    /// The in-flight transfer happened as `txid`: check it, then wait for it to confirm.
    fn sent(
        &self,
        ctx: &Ctx,
        state: &mut RefillState,
        txid: &str,
        outcome: &str,
    ) -> Result<(), CycleError> {
        let f = state
            .in_flight
            .take()
            .expect("called only with an in-flight record");
        state.unconfirmed = Some(txid.to_owned());
        self.save(ctx, state)?;
        log::info!(
            "event=refilled cmd=refill addr={} amount={} txid={txid} sent_today={} \
             outcome={outcome}",
            f.destination,
            f.amount,
            state.sent_today
        );
        let tx = match ctx.wallet().transaction(txid) {
            Ok(tx) => tx,
            Err(e) if e.is_transient() => return Err(e.into()),
            Err(e) => {
                return Err(CycleError::Halt(format!(
                    "refill {txid} cannot be fetched to check it ({e}); verify it by hand"
                )))
            }
        };
        if let Err(e) = txcheck::check_refill(
            &tx,
            &f.destination,
            self.transparent_destination,
            f.amount,
            self.cfg.fee_headroom,
        ) {
            return Err(CycleError::Halt(format!(
                "refill {txid} failed its check: {e:#}"
            )));
        }
        Ok(())
    }

    /// Waits until the last transfer has `wait_confirmations` confirmations.
    fn wait_unconfirmed(
        &self,
        ctx: &Ctx,
        chain: &ChainStatus,
        state: &mut RefillState,
        txid: &str,
    ) -> Result<(), CycleError> {
        let want = u64::from(self.cfg.wait_confirmations);
        let wallet = ctx.wallet();
        let status = match wallet.tx_status(txid, chain.wallet_height)? {
            TxStatus::Confirmed(c) if c >= want => TxStatus::Confirmed(c),
            TxStatus::Gone => TxStatus::Gone,
            _ => {
                log::info!("event=wait_confirmation cmd=refill txid={txid} want={want}");
                wallet.wait_confirmations(txid, want, confirm_timeout(ctx))?
            }
        };
        if status == TxStatus::Gone {
            // Counted against today's cap anyway: under-sending is the safe direction.
            log::warn!("event=refill_gone cmd=refill txid={txid} note=never_confirmed");
        }
        state.unconfirmed = None;
        self.save(ctx, state)
    }
}

/// `refill --resolve-interrupted`: the operator's verdict on a transfer the tool could not
/// settle by itself.
pub fn resolve_interrupted(ctx: &Ctx, resolution: Resolution) -> Exit {
    let run = || -> Result<()> {
        let mut state: RefillState = ctx.state.read_json("state")?.unwrap_or_default();
        state.roll_over(utc_day(now_unix()));
        let f = state
            .in_flight
            .take()
            .context("there is no interrupted refill to resolve")?;
        if resolution == Resolution::NotSent {
            state.refund(&f);
        }
        ctx.state.write_json("state", &state)?;
        log::warn!(
            "event=refill_resolved cmd=refill addr={} amount={} resolution={resolution:?} \
             sent_today={}",
            f.destination,
            f.amount,
            state.sent_today
        );
        Ok(())
    };
    match run() {
        Ok(()) => Exit::Ok,
        Err(e) => {
            log::error!("event=refused cmd=refill reason={e:#}");
            Exit::Refused
        }
    }
}

/// Lines for `--check`.
pub fn check_lines(ctx: &Ctx) -> Result<Vec<String>> {
    let cfg = ctx.cfg.refill.as_ref().expect("validated");
    let mut state: RefillState = ctx.state.read_json("state")?.unwrap_or_default();
    state.roll_over(utc_day(now_unix()));
    let mut lines = vec![format!(
        "refill today: sent={} max_per_day={} max_per_transfer={} mode={:?} destination={}",
        state.sent_today, cfg.max_per_day, cfg.max_per_transfer, cfg.mode, cfg.destination
    )];
    let spendable = ctx
        .wallet()
        .balance(&ctx.cfg.shielded_address, cfg.minconf)?;
    lines.push(format!(
        "refill source spendable (minconf {}): {spendable}",
        cfg.minconf
    ));
    if let Some(f) = &state.in_flight {
        lines.push(format!(
            "INTERRUPTED refill: amount={} destination={} started_unix={} opid={:?}",
            f.amount, f.destination, f.started_unix, f.opid
        ));
    }
    if let Some(t) = &state.unconfirmed {
        lines.push(format!("refill awaiting confirmations: txid={t}"));
    }
    Ok(lines)
}

fn op_timeout(ctx: &Ctx) -> Duration {
    Duration::from_secs(ctx.cfg.general.operation_timeout_secs)
}

fn confirm_timeout(ctx: &Ctx) -> Duration {
    Duration::from_secs(ctx.cfg.general.confirmation_timeout_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roll_over_and_refund() {
        let mut s = RefillState {
            day: 10,
            sent_today: Zat(500),
            ..Default::default()
        };
        s.roll_over(10);
        assert_eq!(s.sent_today, Zat(500));
        let f = InFlight {
            started_unix: 0,
            day: 10,
            amount: Zat(200),
            destination: "t".into(),
            opid: None,
            ops_before: BTreeSet::new(),
            wallet_uptime: None,
        };
        s.refund(&f);
        assert_eq!(s.sent_today, Zat(300));
        s.roll_over(11);
        assert_eq!(s.sent_today, Zat::ZERO);
        // Yesterday's transfer is not refunded against today's total.
        s.sent_today = Zat(100);
        s.refund(&f);
        assert_eq!(s.sent_today, Zat(100));
    }
}
