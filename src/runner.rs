//! The cycle loop shared by both commands: startup checks, the per-cycle safety gate, skipped
//! cycles with capped backoff, the pause after a node rejection, the halt marker, and the exit
//! codes for `--once`.

use std::time::Duration;

use anyhow::anyhow;

use crate::config::Config;
use crate::rpc::{Rpc, RpcError};
use crate::state::{now_unix, StateDir};
use crate::wallet::{ChainStatus, Wallet};

/// Process exit codes. `--once` reports the cycle's result; a loop exits only on refusal or halt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// The cycle completed, including "nothing to do".
    Ok = 0,
    /// Startup refused: configuration, address ownership, lock or halt marker.
    Refused = 1,
    /// The cycle failed or was skipped.
    Failed = 2,
}

pub struct Ctx<'a> {
    pub cfg: &'a Config,
    pub rpc: &'a Rpc,
    pub state: &'a StateDir,
    pub dry_run: bool,
}

impl Ctx<'_> {
    pub fn wallet(&self) -> Wallet<'_> {
        Wallet::new(self.rpc)
    }
}

#[derive(Debug)]
pub enum StartupError {
    /// The configuration or the wallet's answers make running unsafe. The operator must act.
    Refuse(anyhow::Error),
    /// The wallet cannot be reached yet; try again later.
    Unavailable(String),
}

impl From<RpcError> for StartupError {
    fn from(e: RpcError) -> Self {
        if e.is_transient() {
            StartupError::Unavailable(e.to_string())
        } else {
            StartupError::Refuse(anyhow!(e))
        }
    }
}

#[derive(Debug)]
pub enum CycleError {
    /// It is not safe to act now (wallet unreachable, node syncing, wallet behind the tip).
    Skip(String),
    /// The node rejected a transaction: pause this command.
    Rejected(String),
    /// What the wallet did does not match what the tool asked for: stop sending until an
    /// operator has looked.
    Halt(String),
    Failed(anyhow::Error),
}

impl From<RpcError> for CycleError {
    fn from(e: RpcError) -> Self {
        if e.is_transient() {
            CycleError::Skip(e.to_string())
        } else {
            CycleError::Failed(anyhow!(e))
        }
    }
}

impl From<anyhow::Error> for CycleError {
    fn from(e: anyhow::Error) -> Self {
        match e.downcast_ref::<RpcError>() {
            Some(r) if r.is_transient() => CycleError::Skip(format!("{e:#}")),
            _ => CycleError::Failed(e),
        }
    }
}

pub trait Job {
    /// Checks that must pass before the first cycle (address ownership and so on).
    fn startup(&mut self, ctx: &Ctx) -> Result<(), StartupError>;
    /// One cycle. Called only when the chain gate has passed.
    fn cycle(&mut self, ctx: &Ctx, chain: &ChainStatus) -> Result<(), CycleError>;
}

/// The per-cycle gate: skip while the node is syncing or the wallet is behind the node.
pub fn chain_gate(ctx: &Ctx) -> Result<ChainStatus, CycleError> {
    let status = ctx.wallet().chain_status()?;
    // zecd reports its link to the node here; zcashd its peer count. Either way, zero means a
    // transaction built now could not be broadcast.
    let connections = ctx.rpc.read("getconnectioncount", serde_json::json!([]))?;
    if connections.as_u64() == Some(0) {
        return Err(CycleError::Skip(
            "the wallet has no connection to the node or network".into(),
        ));
    }
    if status.initial_block_download {
        return Err(CycleError::Skip(format!(
            "the node or wallet is still syncing (wallet height {}, node height {})",
            status.wallet_height, status.node_height
        )));
    }
    let lag = status.node_height.saturating_sub(status.wallet_height);
    if lag > ctx.cfg.general.max_tip_lag_blocks {
        return Err(CycleError::Skip(format!(
            "the wallet is {lag} blocks behind the node (wallet {}, node {})",
            status.wallet_height, status.node_height
        )));
    }
    Ok(status)
}

/// Whether a failed operation's error is the node refusing the transaction.
pub fn is_node_rejection(code: i64, message: &str) -> bool {
    // -25: RPC_VERIFY_ERROR, -26: RPC_VERIFY_REJECTED, -27: already in chain.
    let m = message.to_ascii_lowercase();
    matches!(code, -25 | -26)
        || m.contains("reject")
        || m.contains("bad-txns")
        || m.contains("consensus")
}

/// Logs and returns the halt reason when sending is halted. An unreadable marker counts as
/// halted.
fn halt_reason(ctx: &Ctx, cmd: &str) -> Option<String> {
    match ctx.state.halted() {
        Ok(None) => None,
        Ok(Some(reason)) => {
            log::error!(
                "event=refused cmd={cmd} reason=halted detail={reason:?} fix=\"investigate, then \
                 delete the marker file\""
            );
            Some(reason)
        }
        Err(e) => {
            log::error!("event=refused cmd={cmd} reason={e:#}");
            Some(format!("{e:#}"))
        }
    }
}

pub fn run(job: &mut dyn Job, ctx: &Ctx, once: bool) -> Exit {
    let cmd = ctx.state_command();
    if halt_reason(ctx, cmd).is_some() {
        return Exit::Refused;
    }

    let g = &ctx.cfg.general;
    let interval = Duration::from_secs(g.interval_secs);
    let backoff = |n: u32| {
        let secs = g
            .interval_secs
            .saturating_mul(1u64 << n.min(16))
            .min(g.max_backoff_secs);
        Duration::from_secs(secs)
    };

    let mut attempts = 0;
    loop {
        match job.startup(ctx) {
            Ok(()) => break,
            // With a cookie file, a 401 means the wallet restarted and is writing a new cookie
            // (the client re-reads it on every 401): wait rather than refuse.
            Err(StartupError::Refuse(e))
                if ctx.cfg.rpc.cookie_file.is_some()
                    && matches!(e.downcast_ref::<RpcError>(), Some(RpcError::Auth(_))) =>
            {
                log::warn!("event=startup_waiting cmd={cmd} reason={e:#}");
                if once {
                    return Exit::Failed;
                }
                std::thread::sleep(backoff(attempts));
                attempts = attempts.saturating_add(1);
            }
            Err(StartupError::Refuse(e)) => {
                log::error!("event=refused cmd={cmd} reason={e:#}");
                return Exit::Refused;
            }
            Err(StartupError::Unavailable(e)) => {
                log::warn!("event=startup_waiting cmd={cmd} reason={e}");
                if once {
                    return Exit::Failed;
                }
                std::thread::sleep(backoff(attempts));
                attempts = attempts.saturating_add(1);
            }
        }
    }
    log::info!("event=started cmd={cmd} dry_run={}", ctx.dry_run);

    let mut skips: u32 = 0;
    let mut failures: u32 = 0;
    loop {
        // An operator (or the other command) may halt sending at any time.
        if halt_reason(ctx, cmd).is_some() {
            return Exit::Refused;
        }
        match ctx.state.paused_until() {
            Ok(None) => {}
            Ok(Some(until)) => {
                let left = until.saturating_sub(now_unix());
                log::warn!("event=paused cmd={cmd} seconds_left={left} reason=node_rejection");
                if once {
                    return Exit::Failed;
                }
                std::thread::sleep(Duration::from_secs(left.clamp(1, g.max_backoff_secs)));
                continue;
            }
            Err(e) => {
                log::error!("event=paused cmd={cmd} reason=unreadable_pause_marker err={e:#}");
                if once {
                    return Exit::Failed;
                }
                std::thread::sleep(Duration::from_secs(g.max_backoff_secs));
                continue;
            }
        }
        let calls_before = ctx.rpc.calls();
        let started = std::time::Instant::now();
        let result = chain_gate(ctx).and_then(|chain| job.cycle(ctx, &chain));
        let rpc_calls = ctx.rpc.calls() - calls_before;
        let millis = started.elapsed().as_millis();
        let sleep = match result {
            Ok(()) => {
                if skips >= g.skipped_cycles_before_error {
                    log::info!("event=recovered cmd={cmd} skipped_cycles={skips}");
                }
                skips = 0;
                failures = 0;
                log::info!("event=cycle_done cmd={cmd} rpc_calls={rpc_calls} millis={millis}");
                if once {
                    return Exit::Ok;
                }
                interval
            }
            Err(CycleError::Skip(reason)) => {
                skips = skips.saturating_add(1);
                if skips == g.skipped_cycles_before_error {
                    log::error!(
                        "event=cycle_skipped cmd={cmd} consecutive={skips} reason={reason:?} \
                         note=\"still skipping; backing off\""
                    );
                } else {
                    log::warn!(
                        "event=cycle_skipped cmd={cmd} consecutive={skips} reason={reason:?}"
                    );
                }
                if once {
                    return Exit::Failed;
                }
                backoff(skips - 1)
            }
            Err(CycleError::Rejected(reason)) => {
                log::error!(
                    "event=node_rejected cmd={cmd} reason={reason:?} paused_seconds={}",
                    g.rejection_pause_secs
                );
                if let Err(e) = ctx
                    .state
                    .pause_until(now_unix().saturating_add(g.rejection_pause_secs))
                {
                    log::error!("event=state_write_failed cmd={cmd} err={e:#}");
                }
                if once {
                    return Exit::Failed;
                }
                // Pause in-process too, so a marker that could not be written cannot turn a
                // rejection into a resubmit loop.
                Duration::from_secs(g.rejection_pause_secs)
            }
            Err(CycleError::Halt(reason)) => {
                log::error!(
                    "event=halted cmd={cmd} reason={reason:?} fix=\"investigate, then delete {}\"",
                    ctx.state.halt_path().display()
                );
                if let Err(e) = ctx.state.halt(&reason) {
                    // Without the marker a restart would resume sending: stay up and idle.
                    log::error!("event=state_write_failed cmd={cmd} err={e:#}");
                    loop {
                        log::error!("event=halted cmd={cmd} reason={reason:?} marker=unwritten");
                        std::thread::sleep(Duration::from_secs(g.max_backoff_secs));
                    }
                }
                return Exit::Failed;
            }
            Err(CycleError::Failed(e)) => {
                failures = failures.saturating_add(1);
                log::error!("event=cycle_failed cmd={cmd} consecutive={failures} err={e:#}");
                if once {
                    return Exit::Failed;
                }
                backoff(failures - 1)
            }
        };
        std::thread::sleep(sleep);
    }
}

impl Ctx<'_> {
    fn state_command(&self) -> &'static str {
        self.state.command().name()
    }
}
