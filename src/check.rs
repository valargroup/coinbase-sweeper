//! `--check`: a read-only reconciliation report an operator can compare against the logs.

use std::io::Write;

use anyhow::Result;

use crate::config::Command;
use crate::refill;
use crate::runner::{Ctx, Exit};
use crate::wallet::{OpState, COINBASE_MATURITY};

pub fn report(ctx: &Ctx, command: Command, out: &mut dyn Write) -> Exit {
    match write_report(ctx, command, out) {
        Ok(()) => Exit::Ok,
        Err(e) => {
            log::error!("event=check_failed cmd={} err={e:#}", command.name());
            Exit::Failed
        }
    }
}

fn write_report(ctx: &Ctx, command: Command, out: &mut dyn Write) -> Result<()> {
    let wallet = ctx.wallet();
    let chain = wallet.chain_status()?;
    writeln!(
        out,
        "chain: wallet_height={} node_height={} syncing={}",
        chain.wallet_height, chain.node_height, chain.initial_block_download
    )?;
    if let Ok(Some(reason)) = ctx.state.halted() {
        writeln!(
            out,
            "HALTED: {reason} (marker {})",
            ctx.state.halt_path().display()
        )?;
    }
    if let Ok(Some(until)) = ctx.state.paused_until() {
        writeln!(out, "paused_until_unix={until}")?;
    }

    let ids = wallet.operation_ids()?;
    let ops = wallet.operations(&ids)?;
    let pending: Vec<_> = ops.iter().filter(|o| !o.state.is_finished()).collect();
    writeln!(out, "pending_operations={}", pending.len())?;
    for op in pending {
        writeln!(
            out,
            "  opid={} method={} state={:?}",
            op.id,
            op.method.as_deref().unwrap_or("?"),
            op.state
        )?;
    }
    for op in ops
        .iter()
        .filter(|o| matches!(o.state, OpState::Failed { .. }))
    {
        writeln!(out, "  finished_failed opid={} state={:?}", op.id, op.state)?;
    }

    let ua = &ctx.cfg.shielded_address;
    let spendable = wallet.balance(ua, 1)?;
    let total = wallet.balance(ua, 0)?;
    writeln!(
        out,
        "ironwood_balance address={ua} confirmed={spendable} including_unconfirmed={total}"
    )?;

    match command {
        Command::Sweep => {
            let cfg = ctx.cfg.sweep.as_ref().expect("validated");
            let all = wallet.list_unspent(1, &cfg.coinbase_addresses)?;
            for addr in &cfg.coinbase_addresses {
                let mine: Vec<_> = all
                    .iter()
                    .filter(|u| &u.address == addr && u.generated)
                    .collect();
                let mature: Vec<_> = mine
                    .iter()
                    .filter(|u| u.confirmations >= COINBASE_MATURITY)
                    .collect();
                writeln!(
                    out,
                    "coinbase address={addr} mature_unswept={} mature_amount={} immature={} \
                     immature_amount={}",
                    mature.len(),
                    mature.iter().map(|u| u.amount).sum::<crate::amount::Zat>(),
                    mine.len() - mature.len(),
                    mine.iter()
                        .filter(|u| u.confirmations < COINBASE_MATURITY)
                        .map(|u| u.amount)
                        .sum::<crate::amount::Zat>(),
                )?;
            }
        }
        Command::Refill => {
            for line in refill::check_lines(ctx)? {
                writeln!(out, "{line}")?;
            }
        }
    }
    Ok(())
}
