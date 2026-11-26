//! `sweep`: shields mature transparent coinbase into the pool's Orchard-only unified address,
//! where it lands as Ironwood notes.

pub mod plan;

use std::collections::HashSet;
use std::time::Duration;

use anyhow::anyhow;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::amount::Zat;
use crate::config::SweepConfig;
use crate::rpc::{RpcError, RPC_WALLET_INSUFFICIENT_FUNDS};
use crate::runner::{is_node_rejection, Ctx, CycleError, Job, StartupError};
use crate::startup;
use crate::txcheck;
use crate::wallet::{ChainStatus, OpState, Operation, TxStatus, COINBASE_MATURITY};

/// zcashd's per-transaction default and the most this tool puts in one sweep.
pub const MAX_INPUTS_PER_SWEEP: u32 = 50;

/// A sweep the tool has seen succeed but that has not yet been checked or confirmed.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct SweepRecord {
    txid: String,
    address: String,
    checked: bool,
}

/// What survives a restart. A sweep stays here until it is checked and has one confirmation, so
/// no new sweep starts while an earlier one could still be holding the same inputs.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SweepState {
    unconfirmed: Vec<SweepRecord>,
    /// Set when the tool lost track of one of its own sweep operations (the wallet restarted or
    /// stopped answering). Its transaction may still be in the mempool unseen, so no new sweep
    /// starts until the wallet has scanned past this height.
    #[serde(default)]
    hold_until_height: Option<u64>,
}

pub struct Sweep {
    cfg: SweepConfig,
}

enum Batch {
    /// `remaining` is the wallet's count of mature inputs left, when it reported one.
    Swept {
        txid: String,
        remaining: Option<u64>,
    },
    NothingToShield,
}

impl Sweep {
    pub fn new(cfg: SweepConfig) -> Sweep {
        Sweep { cfg }
    }
}

impl Job for Sweep {
    fn startup(&mut self, ctx: &Ctx) -> Result<(), StartupError> {
        startup::check_shielded_address(ctx)?;
        let wallet = ctx.wallet();
        for addr in &self.cfg.coinbase_addresses {
            if !startup::is_transparent(ctx, addr)? {
                return Err(StartupError::Refuse(anyhow!(
                    "sweep.coinbase_addresses: {addr} is not a transparent address on this network"
                )));
            }
            if !wallet.is_mine(addr, false)? {
                return Err(StartupError::Refuse(anyhow!(
                    "sweep.coinbase_addresses: the wallet cannot sign for {addr}"
                )));
            }
        }
        if !ctx.dry_run {
            // Operations started by an earlier run must finish before new work starts. Waiting is
            // never a reason to refuse; it is retried.
            wallet
                .settled_operations(op_timeout(ctx))
                .map_err(|e| StartupError::Unavailable(format!("{e:#}")))?;
        }
        Ok(())
    }

    fn cycle(&mut self, ctx: &Ctx, chain: &ChainStatus) -> Result<(), CycleError> {
        let wallet = ctx.wallet();
        if !ctx.dry_run {
            self.settle_previous(ctx)?;
            self.settle_records(ctx, chain)?;
            self.check_hold(ctx, chain)?;
        }
        let utxos = wallet.list_unspent(COINBASE_MATURITY, &self.cfg.coinbase_addresses)?;
        let plans = plan::plan(&utxos, &self.cfg);
        for p in plans.iter().filter(|p| p.alert) {
            log::error!(
                "event=unswept_alert cmd=sweep addr={} mature_inputs={} amount={} threshold={}",
                p.address,
                p.mature_count,
                p.mature_value,
                self.cfg.alert_unswept_zec
            );
        }
        let mut batches_left = self.cfg.max_batches_per_cycle;
        for p in plans.iter().filter(|p| p.due.is_some()) {
            log::info!(
                "event=due cmd=sweep addr={} mature_inputs={} amount={} oldest_wait_blocks={} \
                 reason={:?}",
                p.address,
                p.mature_count,
                p.mature_value,
                p.oldest_wait_blocks,
                p.due
            );
            if ctx.dry_run {
                log::info!(
                    "event=dry_run cmd=sweep addr={} would_shield_inputs={} batches={}",
                    p.address,
                    p.mature_count,
                    p.mature_count.div_ceil(self.cfg.max_inputs as usize)
                );
                continue;
            }
            while batches_left > 0 {
                batches_left -= 1;
                let (txid, remaining) = match self.sweep_batch(ctx, &p.address)? {
                    Batch::NothingToShield => break,
                    Batch::Swept {
                        remaining: Some(0), ..
                    } => break,
                    Batch::Swept { txid, remaining } => (txid, remaining),
                };
                if batches_left == 0 {
                    log::info!(
                        "event=batch_limit cmd=sweep addr={} remaining_inputs={}",
                        p.address,
                        remaining.map_or("unknown".into(), |r| r.to_string())
                    );
                    break;
                }
                // The wallet may not lock coinbase that is being shielded; one confirmation keeps
                // the next batch from selecting the same inputs.
                log::info!("event=wait_confirmation cmd=sweep txid={txid}");
                let status = wallet.wait_confirmations(&txid, 1, confirm_timeout(ctx))?;
                self.forget(ctx, &txid)?;
                if status == TxStatus::Gone {
                    log::warn!("event=sweep_gone cmd=sweep txid={txid} note=inputs_unspent_again");
                    break;
                }
            }
        }
        if plans.iter().all(|p| p.due.is_none()) {
            log::info!(
                "event=nothing_due cmd=sweep mature_inputs={}",
                plans.iter().map(|p| p.mature_count).sum::<usize>()
            );
        }
        Ok(())
    }
}

impl Sweep {
    fn load(&self, ctx: &Ctx) -> Result<SweepState, CycleError> {
        Ok(ctx.state.read_json("state")?.unwrap_or_default())
    }

    fn save(&self, ctx: &Ctx, state: &SweepState) -> Result<(), CycleError> {
        Ok(ctx.state.write_json("state", state)?)
    }

    /// Holds new sweeps until the wallet has seen another block. Best effort on the height: if
    /// the wallet cannot answer now, the hold starts from the height it reports next time.
    fn hold(&self, ctx: &Ctx, opid: &str) -> Result<(), CycleError> {
        let height = ctx.wallet().chain_status().map(|c| c.wallet_height).ok();
        let mut state = self.load(ctx)?;
        state.hold_until_height = Some(height.map_or(0, |h| h + 1));
        log::warn!(
            "event=sweep_hold cmd=sweep opid={opid} until_height={:?} reason=lost_operation",
            state.hold_until_height
        );
        self.save(ctx, &state)
    }

    /// Skips the cycle while a hold is in force; clears it once the wallet is past it.
    fn check_hold(&self, ctx: &Ctx, chain: &ChainStatus) -> Result<(), CycleError> {
        let mut state = self.load(ctx)?;
        let Some(until) = state.hold_until_height else {
            return Ok(());
        };
        // A hold recorded without a height starts counting from now.
        let until = if until == 0 {
            state.hold_until_height = Some(chain.wallet_height + 1);
            self.save(ctx, &state)?;
            chain.wallet_height + 1
        } else {
            until
        };
        if chain.wallet_height < until {
            return Err(CycleError::Skip(format!(
                "waiting for block {until} after a sweep whose outcome was not seen"
            )));
        }
        state.hold_until_height = None;
        self.save(ctx, &state)
    }

    /// Drops a confirmed (or permanently failed) sweep from the state file.
    fn forget(&self, ctx: &Ctx, txid: &str) -> Result<(), CycleError> {
        let mut state = self.load(ctx)?;
        state.unconfirmed.retain(|r| r.txid != txid);
        self.save(ctx, &state)
    }

    /// Waits for every running wallet operation, then accounts for sweeps an earlier run started
    /// but did not see finish (it was killed, or lost its connection): each is logged with its
    /// txid, recorded and checked. Other software's operations are waited for but never cleared.
    fn settle_previous(&self, ctx: &Ctx) -> Result<(), CycleError> {
        let wallet = ctx.wallet();
        for op in wallet.settled_operations(op_timeout(ctx))? {
            let Some(from) = self.own_sweep_source(ctx, &op) else {
                continue;
            };
            wallet.clear_operation(&op.id);
            match op.state {
                OpState::Success { txid } => {
                    log::info!(
                        "event=swept cmd=sweep addr={from} txid={txid} opid={} \
                         outcome=ok_recovered",
                        op.id
                    );
                    self.record_and_check(ctx, &from, &txid)?;
                }
                other => log::warn!(
                    "event=recovered_op cmd=sweep addr={from} opid={} state={other:?}",
                    op.id
                ),
            }
        }
        Ok(())
    }

    /// Checks any recorded sweep not yet checked, and waits for each to confirm.
    fn settle_records(&self, ctx: &Ctx, chain: &ChainStatus) -> Result<(), CycleError> {
        let state = self.load(ctx)?;
        for r in state.unconfirmed {
            if !r.checked {
                self.check(ctx, &r.address, &r.txid)?;
            }
            let status = match ctx.wallet().tx_status(&r.txid, chain.wallet_height)? {
                TxStatus::Pending => {
                    log::info!(
                        "event=wait_confirmation cmd=sweep txid={} note=previous_sweep",
                        r.txid
                    );
                    ctx.wallet()
                        .wait_confirmations(&r.txid, 1, confirm_timeout(ctx))?
                }
                s => s,
            };
            if status == TxStatus::Gone {
                log::warn!(
                    "event=sweep_gone cmd=sweep addr={} txid={} note=inputs_unspent_again",
                    r.address,
                    r.txid
                );
            }
            self.forget(ctx, &r.txid)?;
        }
        Ok(())
    }

    /// The source address when `op` is a sweep this tool would have started.
    fn own_sweep_source(&self, ctx: &Ctx, op: &Operation) -> Option<String> {
        if op.method.as_deref() != Some("z_shieldcoinbase") {
            return None;
        }
        let from = op.params.get("fromaddress")?.as_str()?;
        let to = op.params.get("toaddress")?.as_str()?;
        (to == ctx.cfg.shielded_address && self.cfg.coinbase_addresses.iter().any(|a| a == from))
            .then(|| from.to_owned())
    }

    /// Records a sweep (before anything else can fail) and checks it.
    fn record_and_check(&self, ctx: &Ctx, address: &str, txid: &str) -> Result<(), CycleError> {
        let mut state = self.load(ctx)?;
        if !state.unconfirmed.iter().any(|r| r.txid == txid) {
            state.unconfirmed.push(SweepRecord {
                txid: txid.to_owned(),
                address: address.to_owned(),
                checked: false,
            });
            self.save(ctx, &state)?;
        }
        self.check(ctx, address, txid)
    }

    /// Checks a recorded sweep and marks it checked. A transaction that fails the check, or that
    /// can no longer be found to check, halts the command.
    fn check(&self, ctx: &Ctx, address: &str, txid: &str) -> Result<(), CycleError> {
        let tx = match ctx.wallet().transaction(txid) {
            Ok(tx) => tx,
            Err(e) if e.is_transient() => return Err(e.into()),
            Err(e) => {
                return Err(CycleError::Halt(format!(
                    "sweep {txid} from {address} cannot be fetched to check it ({e}); verify it \
                     by hand"
                )))
            }
        };
        if let Err(e) = txcheck::check_sweep(&tx) {
            return Err(CycleError::Halt(format!(
                "sweep {txid} from {address} failed its check: {e:#}"
            )));
        }
        let mut state = self.load(ctx)?;
        for r in state.unconfirmed.iter_mut().filter(|r| r.txid == txid) {
            r.checked = true;
        }
        self.save(ctx, &state)
    }

    fn sweep_batch(&self, ctx: &Ctx, address: &str) -> Result<Batch, CycleError> {
        let wallet = ctx.wallet();
        let ops_before: HashSet<String> = wallet.operation_ids()?.into_iter().collect();
        let params = json!([
            address,
            ctx.cfg.shielded_address,
            Value::Null,
            self.cfg.max_inputs.min(MAX_INPUTS_PER_SWEEP)
        ]);
        let reply = match ctx.rpc.submit("z_shieldcoinbase", params) {
            Ok(v) => v,
            Err(RpcError::Rpc { code, message }) if code == RPC_WALLET_INSUFFICIENT_FUNDS => {
                log::info!("event=nothing_to_shield cmd=sweep addr={address} detail={message:?}");
                return Ok(Batch::NothingToShield);
            }
            Err(e) if e.outcome_unknown() => {
                return Err(self.unknown(ctx, address, &ops_before, e))
            }
            // Unreachable, still warming up, or a definite refusal: the wallet did not act.
            Err(e) => return Err(e.into()),
        };
        let Some(opid) = reply.get("opid").and_then(Value::as_str).map(str::to_owned) else {
            let e = RpcError::Protocol(format!("z_shieldcoinbase returned no opid: {reply}"));
            return Err(self.unknown(ctx, address, &ops_before, e));
        };
        let inputs = reply.get("shieldingUTXOs").and_then(Value::as_u64);
        let value = reply
            .get("shieldingValue")
            .and_then(|v| Zat::from_json(v).ok());
        let remaining = reply.get("remainingUTXOs").and_then(Value::as_u64);
        let shown = |v: Option<String>| v.unwrap_or_else(|| "unknown".into());
        let inputs = shown(inputs.map(|n| n.to_string()));
        let value = shown(value.map(|z| z.to_string()));
        log::info!(
            "event=sweep_submitted cmd=sweep addr={address} inputs={inputs} amount={value} \
             remaining_inputs={} opid={opid}",
            shown(remaining.map(|n| n.to_string())),
        );
        let txid = self.finish_operation(ctx, address, &opid)?;
        log::info!(
            "event=swept cmd=sweep addr={address} inputs={inputs} amount={value} txid={txid} \
             outcome=ok"
        );
        Ok(Batch::Swept { txid, remaining })
    }

    /// Waits for a sweep operation, then records and checks its transaction.
    fn finish_operation(&self, ctx: &Ctx, address: &str, opid: &str) -> Result<String, CycleError> {
        let state = match ctx.wallet().wait_operation(opid, op_timeout(ctx), true) {
            Ok(Some(state)) => state,
            Ok(None) => {
                self.hold(ctx, opid)?;
                return Err(CycleError::Failed(anyhow!(
                    "the wallet lost operation {opid} (did it restart?); new sweeps wait for the \
                     next block"
                )));
            }
            Err(e) => {
                self.hold(ctx, opid)?;
                return Err(e.into());
            }
        };
        let txid = match state {
            OpState::Success { txid } => txid,
            OpState::Failed { code, message } if is_node_rejection(code, &message) => {
                log::error!(
                    "event=sweep_failed cmd=sweep addr={address} opid={opid} code={code} \
                     reason={message:?} outcome=rejected"
                );
                return Err(CycleError::Rejected(message));
            }
            OpState::Failed { code, message } => {
                return Err(CycleError::Failed(anyhow!(
                    "sweep operation {opid} for {address} failed ({code}): {message}"
                )))
            }
            other => {
                return Err(CycleError::Failed(anyhow!(
                    "sweep operation {opid} ended as {other:?}"
                )))
            }
        };
        self.record_and_check(ctx, address, &txid)?;
        Ok(txid)
    }

    /// A submission whose outcome is unknown: settle any sweep of ours that appeared, then end
    /// the cycle. Never resend.
    fn unknown(
        &self,
        ctx: &Ctx,
        address: &str,
        ops_before: &HashSet<String>,
        e: RpcError,
    ) -> CycleError {
        log::error!("event=submit_unknown cmd=sweep addr={address} err={e}");
        let settle = || -> Result<(), CycleError> {
            let wallet = ctx.wallet();
            let ids: Vec<String> = wallet
                .operation_ids()?
                .into_iter()
                .filter(|id| !ops_before.contains(id))
                .collect();
            let ours: Vec<Operation> = wallet
                .operations(&ids)?
                .into_iter()
                .filter(|op| self.own_sweep_source(ctx, op).as_deref() == Some(address))
                .collect();
            if ours.is_empty() {
                log::warn!("event=submit_unknown_settled cmd=sweep addr={address} new_sweeps=0");
            }
            for op in ours {
                let txid = self.finish_operation(ctx, address, &op.id)?;
                log::info!(
                    "event=swept cmd=sweep addr={address} txid={txid} opid={} \
                     outcome=ok_after_unknown",
                    op.id
                );
            }
            Ok(())
        };
        match settle() {
            Ok(()) => CycleError::Failed(anyhow!(
                "the z_shieldcoinbase call for {address} got no usable answer ({e}); not \
                 resending in this cycle"
            )),
            Err(other) => other,
        }
    }
}

fn op_timeout(ctx: &Ctx) -> Duration {
    Duration::from_secs(ctx.cfg.general.operation_timeout_secs)
}

fn confirm_timeout(ctx: &Ctx) -> Duration {
    Duration::from_secs(ctx.cfg.general.confirmation_timeout_secs)
}
