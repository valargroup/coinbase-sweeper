//! Command-line entry point: parses flags, loads the configuration, takes the lock and hands
//! over to the runner.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

use crate::config::{Command, Config, PASSWORD_ENV};
use crate::rpc::Rpc;
use crate::runner::{self, Ctx, Exit, Job};
use crate::state::StateDir;
use crate::{check, refill, sweep};

#[derive(Parser, Debug)]
#[command(
    name = "coinbase-sweeper",
    version,
    about = "Sweep a pool's transparent coinbase into Ironwood and refill its payout wallet"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Shield mature transparent coinbase into the pool's Orchard-only unified address.
    Sweep(Common),
    /// Move funds from the pool's unified address to its payout destination.
    Refill {
        #[command(flatten)]
        common: Common,
        /// Settle a refill that was interrupted and could not be settled automatically, after
        /// checking the wallet by hand: `sent` if the transfer reached the destination,
        /// `not-sent` if it did not.
        #[arg(long, value_name = "sent|not-sent")]
        resolve_interrupted: Option<refill::Resolution>,
    },
}

#[derive(Args, Debug)]
struct Common {
    /// Path to the configuration file.
    #[arg(long, value_name = "FILE")]
    config: PathBuf,
    /// Report what one cycle would do, and send nothing.
    #[arg(long)]
    dry_run: bool,
    /// Run one cycle and exit (0 done, 1 refused at startup, 2 cycle failed or skipped).
    #[arg(long)]
    once: bool,
    /// Print balances, pending operations and today's refill total, then exit.
    #[arg(long)]
    check: bool,
}

pub fn main(args: impl IntoIterator<Item = OsString>) -> Exit {
    init_logging();
    let cli = match Cli::try_parse_from(args) {
        Ok(c) => c,
        Err(e) => {
            let _ = e.print();
            return if e.use_stderr() {
                Exit::Refused
            } else {
                Exit::Ok
            };
        }
    };
    let (command, common, resolution) = match cli.command {
        Cmd::Sweep(c) => (Command::Sweep, c, None),
        Cmd::Refill {
            common,
            resolve_interrupted,
        } => (Command::Refill, common, resolve_interrupted),
    };
    let cmd = command.name();
    let refuse = |e: anyhow::Error| {
        log::error!("event=refused cmd={cmd} reason={e:#}");
        Exit::Refused
    };

    let cfg = match Config::load(&common.config).and_then(|c| {
        c.validate_for(command)?;
        Ok(c)
    }) {
        Ok(c) => c,
        Err(e) => return refuse(e),
    };
    let credentials = match cfg.rpc.credentials(std::env::var(PASSWORD_ENV).ok()) {
        Ok(c) => c,
        Err(e) => return refuse(e),
    };
    let rpc = Rpc::new(
        &cfg.rpc.url,
        &credentials,
        cfg.rpc.cookie_file.clone(),
        cfg.read_timeout(),
        cfg.submit_timeout(),
        cfg.rpc.read_retries,
    );
    let state = match StateDir::new(&cfg.general.state_dir, command) {
        Ok(s) => s,
        Err(e) => return refuse(e),
    };
    let ctx = Ctx {
        cfg: &cfg,
        rpc: &rpc,
        state: &state,
        dry_run: common.dry_run,
    };

    if common.check {
        return check::report(&ctx, command, &mut std::io::stdout());
    }

    // Read-only modes skip the lock so they can run next to the service.
    let _lock = if common.dry_run {
        None
    } else {
        match state.lock() {
            Ok(l) => Some(l),
            Err(e) => return refuse(e),
        }
    };

    if let Some(resolution) = resolution {
        return refill::resolve_interrupted(&ctx, resolution);
    }

    let mut job: Box<dyn Job> = match command {
        Command::Sweep => Box::new(sweep::Sweep::new(
            cfg.sweep
                .clone()
                .expect("validate_for(Sweep) requires [sweep]"),
        )),
        Command::Refill => Box::new(refill::Refill::new(
            cfg.refill
                .clone()
                .expect("validate_for(Refill) requires [refill]"),
        )),
    };
    runner::run(job.as_mut(), &ctx, common.once || common.dry_run)
}

fn init_logging() {
    let env = env_logger::Env::default().default_filter_or("info");
    let _ = env_logger::Builder::from_env(env)
        .format(|buf, record| {
            writeln!(
                buf,
                "{} {} {}",
                buf.timestamp_millis(),
                record.level(),
                record.args()
            )
        })
        .try_init();
}
