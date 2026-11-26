//! `sweep`: shields mature transparent coinbase into the pool's Orchard-only unified address,
//! where it lands as Ironwood notes.

pub mod plan;

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{anyhow, Context};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::amount::Zat;
use crate::config::SweepConfig;
use crate::rpc::{RpcError, RPC_WALLET_INSUFFICIENT_FUNDS};
use crate::runner::{is_node_rejection, Ctx, CycleError, Job, StartupError};
use crate::startup;
use crate::txcheck;
use crate::wallet::{ChainStatus, OpState, COINBASE_MATURITY};

/// zcashd's per-transaction default and the most this tool puts in one sweep.
pub const MAX_INPUTS_PER_SWEEP: u32 = 50;

/// What survives a restart: the last sweep, so the next batch waits for it to confirm.
#[derive(Debug, Default, Serialize, Deserialize)]
struct SweepState {
    last_txid: Option<String>,
}

pub struct Sweep {
    cfg: SweepConfig,
}

impl Sweep {
    pub fn new(cfg: SweepConfig) -> Sweep {
        Sweep { cfg }
    }
}

enum Batch {
    Swept { txid: String, remaining: u64 },
    NothingToShield,
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
            // Operations started by an earlier run must finish before new work starts.
            let waited = wallet
                .wait_all_pending(op_timeout(ctx))
                .map_err(startup_error)?;
            if waited > 0 {
                log::info!("event=startup_pending_ops_done cmd=sweep count={waited}");
            }
        }
        Ok(())
    }

    fn cycle(&mut self, ctx: &Ctx, _chain: &ChainStatus) -> Result<(), CycleError> {
        let wallet = ctx.wallet();
        if !ctx.dry_run {
            wallet.wait_all_pending(op_timeout(ctx))?;
            self.wait_last_sweep(ctx)?;
        }
        let utxos = wallet.list_unspent(COINBASE_MATURITY, &self.cfg.coinbase_addresses)?;
        let plans = plan::plan(&utxos, &self.cfg);
        for p in &plans {
            if p.alert {
                log::error!(
                    "event=unswept_alert cmd=sweep addr={} mature_inputs={} amount={} \
                     threshold={}",
                    p.address,
                    p.mature_count,
                    p.mature_value,
                    self.cfg.alert_unswept_zec
                );
            }
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
                let per = self.cfg.max_inputs as usize;
                log::info!(
                    "event=dry_run cmd=sweep addr={} would_shield_inputs={} batches={}",
                    p.address,
                    p.mature_count,
                    p.mature_count.div_ceil(per)
                );
                continue;
            }
            while batches_left > 0 {
                batches_left -= 1;
                match self.sweep_batch(ctx, &p.address)? {
                    Batch::NothingToShield => break,
                    Batch::Swept { remaining: 0, .. } => break,
                    Batch::Swept { txid, remaining } => {
                        if batches_left == 0 {
                            log::info!(
                                "event=batch_limit cmd=sweep addr={} remaining_inputs={remaining}",
                                p.address
                            );
                            break;
                        }
                        // The wallet may not lock coinbase that is being shielded; one
                        // confirmation keeps the next batch from selecting the same inputs.
                        log::info!("event=wait_confirmation cmd=sweep txid={txid}");
                        wallet.wait_confirmations(&txid, 1, confirm_timeout(ctx))?;
                    }
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
    /// Waits for the previous run's last sweep to confirm before selecting inputs again.
    fn wait_last_sweep(&self, ctx: &Ctx) -> Result<(), CycleError> {
        let state: SweepState = ctx.state.read_json("state")?.unwrap_or_default();
        let Some(txid) = state.last_txid else {
            return Ok(());
        };
        match ctx.wallet().confirmations(&txid) {
            Ok(c) if c >= 1 => {}
            Ok(_) => {
                log::info!("event=wait_confirmation cmd=sweep txid={txid} note=previous_sweep");
                ctx.wallet()
                    .wait_confirmations(&txid, 1, confirm_timeout(ctx))?;
            }
            // The wallet no longer knows the transaction (it expired or was dropped): its inputs
            // are unspent again and will be listed.
            Err(RpcError::Rpc { code: -5, .. }) => {
                log::warn!("event=previous_sweep_unknown cmd=sweep txid={txid}");
            }
            Err(e) => return Err(e.into()),
        }
        ctx.state
            .write_json("state", &SweepState { last_txid: None })?;
        Ok(())
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
            Err(e @ RpcError::NoAnswer(_)) => {
                // The wallet may or may not have started the sweep. Never resend: settle any
                // operation it did start, then end the cycle.
                log::error!("event=submit_unknown cmd=sweep addr={address} err={e}");
                self.settle_unknown(ctx, address, &ops_before)?;
                return Err(CycleError::Failed(anyhow!(
                    "the z_shieldcoinbase call for {address} got no answer; not resending in \
                     this cycle"
                )));
            }
            Err(e) => return Err(e.into()),
        };
        let opid = reply
            .get("opid")
            .and_then(Value::as_str)
            .context("z_shieldcoinbase returned no opid")?
            .to_owned();
        let inputs = reply
            .get("shieldingUTXOs")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let value = reply
            .get("shieldingValue")
            .map(Zat::from_json)
            .transpose()?
            .unwrap_or_default();
        let remaining = reply
            .get("remainingUTXOs")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        log::info!(
            "event=sweep_submitted cmd=sweep addr={address} inputs={inputs} amount={value} \
             remaining_inputs={remaining} opid={opid}"
        );
        let txid = self.finish_operation(ctx, address, &opid)?;
        log::info!(
            "event=swept cmd=sweep addr={address} inputs={inputs} amount={value} txid={txid} \
             outcome=ok"
        );
        Ok(Batch::Swept { txid, remaining })
    }

    /// Waits for a sweep operation, records its transaction and checks it.
    fn finish_operation(&self, ctx: &Ctx, address: &str, opid: &str) -> Result<String, CycleError> {
        let state = ctx
            .wallet()
            .wait_operation(opid, op_timeout(ctx))?
            .ok_or_else(|| {
                CycleError::Failed(anyhow!(
                    "the wallet lost operation {opid} (did it restart?); its outcome will show \
                     in the next cycle's listing"
                ))
            })?;
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
        ctx.state.write_json(
            "state",
            &SweepState {
                last_txid: Some(txid.clone()),
            },
        )?;
        // Check now, while the transaction is in the mempool: a node without a transaction
        // index cannot find it later.
        let tx = ctx.wallet().raw_transaction(&txid)?;
        if let Err(e) = txcheck::check_sweep(&tx) {
            return Err(CycleError::Halt(format!(
                "sweep {txid} from {address} failed its check: {e:#}"
            )));
        }
        Ok(txid)
    }

    /// After a submission with no answer: wait for and check any operation that appeared.
    fn settle_unknown(
        &self,
        ctx: &Ctx,
        address: &str,
        ops_before: &HashSet<String>,
    ) -> Result<(), CycleError> {
        let new_ops: Vec<String> = ctx
            .wallet()
            .operation_ids()?
            .into_iter()
            .filter(|id| !ops_before.contains(id))
            .collect();
        if new_ops.is_empty() {
            log::warn!("event=submit_unknown_settled cmd=sweep addr={address} new_operations=0");
        }
        for opid in new_ops {
            let txid = self.finish_operation(ctx, address, &opid)?;
            log::info!("event=swept cmd=sweep addr={address} txid={txid} outcome=ok_after_unknown");
        }
        Ok(())
    }
}

fn op_timeout(ctx: &Ctx) -> Duration {
    Duration::from_secs(ctx.cfg.general.operation_timeout_secs)
}

fn confirm_timeout(ctx: &Ctx) -> Duration {
    Duration::from_secs(ctx.cfg.general.confirmation_timeout_secs)
}

/// Waiting on an earlier operation is never a reason to refuse; it is retried.
fn startup_error(e: anyhow::Error) -> StartupError {
    StartupError::Unavailable(format!("{e:#}"))
}
