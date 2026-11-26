//! `refill`: not built yet in this iteration.

use anyhow::{anyhow, Result};

use crate::config::RefillConfig;
use crate::runner::{Ctx, CycleError, Exit, Job, StartupError};
use crate::wallet::ChainStatus;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Resolution {
    Sent,
    NotSent,
}

pub struct Refill;

impl Refill {
    pub fn new(_cfg: RefillConfig) -> Refill {
        Refill
    }
}

impl Job for Refill {
    fn startup(&mut self, _ctx: &Ctx) -> Result<(), StartupError> {
        Err(StartupError::Refuse(anyhow!(
            "refill is not implemented yet"
        )))
    }

    fn cycle(&mut self, _ctx: &Ctx, _chain: &ChainStatus) -> Result<(), CycleError> {
        unreachable!("startup always refuses")
    }
}

pub fn resolve_interrupted(_ctx: &Ctx, _r: Resolution) -> Exit {
    Exit::Refused
}

pub fn check_lines(_ctx: &Ctx) -> Result<Vec<String>> {
    Ok(Vec::new())
}
