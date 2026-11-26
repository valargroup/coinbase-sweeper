//! coinbase-sweeper: moves a mining pool's transparent coinbase into Ironwood (`sweep`) and
//! funds the pool's payout wallet from Ironwood (`refill`), through a zcashd-compatible wallet
//! RPC. The wallet holds the keys and signs; this tool never sees them.

pub mod amount;
pub mod check;
pub mod cli;
pub mod config;
pub mod refill;
pub mod rpc;
pub mod runner;
pub mod startup;
pub mod state;
pub mod sweep;
pub mod txcheck;
pub mod wallet;
