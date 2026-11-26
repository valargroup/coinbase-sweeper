//! The configuration file. Every key is documented in docs/CONFIGURATION.md; a test keeps that
//! document, `sweeper.example.toml` and this schema in step.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Deserializer};

use crate::amount::Zat;

/// The environment variable that carries the RPC password when no cookie file is configured.
pub const PASSWORD_ENV: &str = "SWEEPER_RPC_PASSWORD";

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub rpc: RpcConfig,
    #[serde(default)]
    pub general: GeneralConfig,
    /// The pool's Orchard-only unified address: where `sweep` sends coinbase and where `refill`
    /// spends from.
    pub shielded_address: String,
    pub sweep: Option<SweepConfig>,
    pub refill: Option<RefillConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RpcConfig {
    pub url: String,
    #[serde(default)]
    pub user: Option<String>,
    /// A zcashd-style cookie file (`user:password`). When absent, the password comes from the
    /// `SWEEPER_RPC_PASSWORD` environment variable.
    #[serde(default)]
    pub cookie_file: Option<PathBuf>,
    #[serde(default = "defaults::read_timeout")]
    pub timeout_secs: u64,
    #[serde(default = "defaults::submit_timeout")]
    pub submit_timeout_secs: u64,
    #[serde(default = "defaults::read_retries")]
    pub read_retries: u32,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GeneralConfig {
    pub state_dir: PathBuf,
    pub interval_secs: u64,
    pub max_tip_lag_blocks: u64,
    pub operation_timeout_secs: u64,
    pub confirmation_timeout_secs: u64,
    pub skipped_cycles_before_error: u32,
    pub max_backoff_secs: u64,
    pub rejection_pause_secs: u64,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        GeneralConfig {
            state_dir: PathBuf::from("/var/lib/coinbase-sweeper"),
            interval_secs: 60,
            max_tip_lag_blocks: 2,
            operation_timeout_secs: 600,
            confirmation_timeout_secs: 1800,
            skipped_cycles_before_error: 5,
            max_backoff_secs: 600,
            rejection_pause_secs: 3600,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SweepConfig {
    pub coinbase_addresses: Vec<String>,
    #[serde(default = "defaults::min_inputs")]
    pub min_inputs: u32,
    #[serde(default = "defaults::max_wait_blocks")]
    pub max_wait_blocks: u32,
    #[serde(default = "defaults::max_inputs")]
    pub max_inputs: u32,
    #[serde(default = "defaults::max_batches_per_cycle")]
    pub max_batches_per_cycle: u32,
    #[serde(deserialize_with = "zec")]
    pub alert_unswept_zec: Zat,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RefillMode {
    TopUp,
    DrainAbove,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefillConfig {
    pub mode: RefillMode,
    pub destination: String,
    /// Must repeat `destination` when the wallet does not own it.
    #[serde(default)]
    pub external_destination: Option<String>,
    #[serde(default, deserialize_with = "opt_zec")]
    pub low_water: Option<Zat>,
    #[serde(default, deserialize_with = "opt_zec")]
    pub target: Option<Zat>,
    #[serde(default, deserialize_with = "opt_zec")]
    pub reserve: Option<Zat>,
    #[serde(default = "defaults::minconf")]
    pub minconf: u32,
    #[serde(default = "defaults::fee_headroom", deserialize_with = "zec")]
    pub fee_headroom: Zat,
    #[serde(default = "defaults::min_transfer", deserialize_with = "zec")]
    pub min_transfer: Zat,
    #[serde(deserialize_with = "zec")]
    pub max_per_transfer: Zat,
    #[serde(deserialize_with = "zec")]
    pub max_per_day: Zat,
    #[serde(default = "defaults::wait_confirmations")]
    pub wait_confirmations: u32,
}

mod defaults {
    use crate::amount::Zat;
    pub fn read_timeout() -> u64 {
        30
    }
    pub fn submit_timeout() -> u64 {
        120
    }
    pub fn read_retries() -> u32 {
        3
    }
    pub fn min_inputs() -> u32 {
        10
    }
    pub fn max_wait_blocks() -> u32 {
        100
    }
    pub fn max_inputs() -> u32 {
        50
    }
    pub fn max_batches_per_cycle() -> u32 {
        10
    }
    pub fn minconf() -> u32 {
        10
    }
    pub fn fee_headroom() -> Zat {
        Zat(100_000)
    }
    pub fn min_transfer() -> Zat {
        Zat(1_000_000)
    }
    pub fn wait_confirmations() -> u32 {
        1
    }
}

/// Amounts may be written as TOML numbers (`1.5`) or strings (`"1.5"`); both are parsed as exact
/// decimals.
fn zec<'de, D: Deserializer<'de>>(d: D) -> Result<Zat, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Raw {
        Text(String),
        Int(u64),
        Float(f64),
    }
    let text = match Raw::deserialize(d)? {
        Raw::Text(s) => s,
        Raw::Int(i) => i.to_string(),
        // `{}` prints the shortest text that reads back as the same float, so 0.001 stays 0.001.
        Raw::Float(f) => format!("{f}"),
    };
    Zat::parse_zec(&text).map_err(serde::de::Error::custom)
}

fn opt_zec<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Zat>, D::Error> {
    zec(d).map(Some)
}

/// Which command is running; each validates its own section.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    Sweep,
    Refill,
}

impl Command {
    pub fn name(self) -> &'static str {
        match self {
            Command::Sweep => "sweep",
            Command::Refill => "refill",
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config file {}", path.display()))?;
        Config::parse(&text).with_context(|| format!("invalid config file {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Config> {
        let cfg: Config = toml::from_str(text)?;
        cfg.validate_common()?;
        Ok(cfg)
    }

    fn validate_common(&self) -> Result<()> {
        ensure!(
            self.rpc.url.starts_with("http://"),
            "rpc.url must start with http:// (TLS is not supported; keep the wallet RPC on \
             localhost or a private network)"
        );
        ensure!(
            (1..=3600).contains(&self.rpc.timeout_secs),
            "rpc.timeout_secs must be 1-3600"
        );
        ensure!(
            (1..=3600).contains(&self.rpc.submit_timeout_secs),
            "rpc.submit_timeout_secs must be 1-3600"
        );
        ensure!(self.rpc.read_retries <= 10, "rpc.read_retries must be 0-10");
        let g = &self.general;
        ensure!(
            (1..=86_400).contains(&g.interval_secs),
            "general.interval_secs must be 1-86400"
        );
        ensure!(
            g.max_tip_lag_blocks <= 100,
            "general.max_tip_lag_blocks must be 0-100"
        );
        ensure!(
            (10..=86_400).contains(&g.operation_timeout_secs),
            "general.operation_timeout_secs must be 10-86400"
        );
        ensure!(
            (10..=86_400).contains(&g.confirmation_timeout_secs),
            "general.confirmation_timeout_secs must be 10-86400"
        );
        ensure!(
            (60..=604_800).contains(&g.rejection_pause_secs),
            "general.rejection_pause_secs must be 60-604800"
        );
        ensure!(
            g.max_backoff_secs <= 86_400,
            "general.max_backoff_secs must be at most 86400"
        );
        ensure!(
            g.skipped_cycles_before_error >= 1,
            "general.skipped_cycles_before_error must be at least 1"
        );
        ensure!(
            g.max_backoff_secs >= g.interval_secs,
            "general.max_backoff_secs must be at least general.interval_secs"
        );
        ensure!(
            !self.shielded_address.trim().is_empty(),
            "shielded_address is required"
        );
        Ok(())
    }

    /// Checks the section the command needs. Called once at startup.
    pub fn validate_for(&self, command: Command) -> Result<()> {
        match command {
            Command::Sweep => {
                let s = self
                    .sweep
                    .as_ref()
                    .context("the [sweep] section is missing")?;
                ensure!(
                    !s.coinbase_addresses.is_empty(),
                    "sweep.coinbase_addresses must list at least one address"
                );
                let mut seen = std::collections::HashSet::new();
                for a in &s.coinbase_addresses {
                    ensure!(seen.insert(a), "sweep.coinbase_addresses lists {a} twice");
                }
                ensure!(
                    (1..=50).contains(&s.max_inputs),
                    "sweep.max_inputs must be 1-50"
                );
                ensure!(
                    (1..=s.max_inputs).contains(&s.min_inputs),
                    "sweep.min_inputs must be between 1 and sweep.max_inputs"
                );
                ensure!(
                    (1..=100).contains(&s.max_batches_per_cycle),
                    "sweep.max_batches_per_cycle must be 1-100"
                );
            }
            Command::Refill => {
                let r = self
                    .refill
                    .as_ref()
                    .context("the [refill] section is missing")?;
                ensure!(
                    !r.destination.trim().is_empty(),
                    "refill.destination is required"
                );
                ensure!(
                    r.destination != self.shielded_address,
                    "refill.destination must differ from shielded_address"
                );
                if let Some(ext) = &r.external_destination {
                    ensure!(
                        ext == &r.destination,
                        "refill.external_destination ({ext}) does not match refill.destination \
                         ({}); they must be the same address, written twice",
                        r.destination
                    );
                }
                match r.mode {
                    RefillMode::TopUp => {
                        let low = r.low_water.context("top_up mode needs refill.low_water")?;
                        let target = r.target.context("top_up mode needs refill.target")?;
                        ensure!(
                            target > low,
                            "refill.target must be greater than refill.low_water"
                        );
                        ensure!(
                            r.reserve.is_none(),
                            "refill.reserve applies to drain_above mode only"
                        );
                    }
                    RefillMode::DrainAbove => {
                        ensure!(r.reserve.is_some(), "drain_above mode needs refill.reserve");
                        ensure!(
                            r.low_water.is_none() && r.target.is_none(),
                            "refill.low_water and refill.target apply to top_up mode only"
                        );
                    }
                }
                ensure!(
                    r.max_per_transfer > Zat::ZERO,
                    "refill.max_per_transfer must be positive"
                );
                ensure!(
                    r.max_per_day >= r.max_per_transfer,
                    "refill.max_per_day must be at least refill.max_per_transfer"
                );
                ensure!(
                    r.min_transfer > Zat::ZERO && r.min_transfer <= r.max_per_transfer,
                    "refill.min_transfer must be positive and at most refill.max_per_transfer"
                );
                ensure!(
                    r.fee_headroom >= Zat(10_000) && r.fee_headroom <= Zat(100_000_000),
                    "refill.fee_headroom must be between 0.0001 and 1 ZEC"
                );
                ensure!(
                    (1..=1000).contains(&r.minconf),
                    "refill.minconf must be 1-1000"
                );
                ensure!(
                    (1..=100).contains(&r.wait_confirmations),
                    "refill.wait_confirmations must be 1-100"
                );
            }
        }
        Ok(())
    }

    pub fn read_timeout(&self) -> Duration {
        Duration::from_secs(self.rpc.timeout_secs)
    }

    pub fn submit_timeout(&self) -> Duration {
        Duration::from_secs(self.rpc.submit_timeout_secs)
    }
}

/// RPC credentials, resolved once at startup.
#[derive(Clone)]
pub struct Credentials {
    pub user: String,
    pub password: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Credentials {{ user: {:?}, password: <hidden> }}",
            self.user
        )
    }
}

impl RpcConfig {
    /// The cookie file wins when configured; otherwise the user from the config and the password
    /// from `SWEEPER_RPC_PASSWORD`.
    pub fn credentials(&self, env_password: Option<String>) -> Result<Credentials> {
        if let Some(path) = &self.cookie_file {
            return read_cookie(path);
        }
        let user = self
            .user
            .clone()
            .context("set rpc.user and SWEEPER_RPC_PASSWORD, or rpc.cookie_file")?;
        let password = match env_password {
            Some(p) if !p.is_empty() => p,
            _ => bail!("{PASSWORD_ENV} is not set (or set rpc.cookie_file)"),
        };
        Ok(Credentials { user, password })
    }
}

/// Reads a zcashd-style cookie file (`user:password`).
pub fn read_cookie(path: &Path) -> Result<Credentials> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read rpc.cookie_file {}", path.display()))?;
    let (user, password) = text
        .trim()
        .split_once(':')
        .context("rpc.cookie_file must hold user:password")?;
    Ok(Credentials {
        user: user.to_owned(),
        password: password.to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"
shielded_address = "uregtest1abc"
[rpc]
url = "http://127.0.0.1:8232/"
user = "pool"
[general]
state_dir = "/tmp/x"
"#;

    fn with(extra: &str) -> Result<Config> {
        Config::parse(&format!("{BASE}{extra}"))
    }

    #[test]
    fn sweep_defaults() {
        let c = with("[sweep]\ncoinbase_addresses = [\"t1\"]\nalert_unswept_zec = 100\n").unwrap();
        c.validate_for(Command::Sweep).unwrap();
        let s = c.sweep.unwrap();
        assert_eq!(
            (s.min_inputs, s.max_inputs, s.max_batches_per_cycle),
            (10, 50, 10)
        );
        assert_eq!(s.alert_unswept_zec, Zat::parse_zec("100").unwrap());
    }

    #[test]
    fn refuses_password_in_file_and_unknown_keys() {
        let e = Config::parse(&BASE.replace("user = \"pool\"", "password = \"x\"")).unwrap_err();
        assert!(format!("{e:#}").contains("password"), "{e:#}");
        assert!(with("[sweep]\nbogus = 1\n").is_err());
    }

    #[test]
    fn refill_validation() {
        let ok = "[refill]\nmode = \"top_up\"\ndestination = \"tdest\"\nlow_water = 1\n\
                  target = 5\nmax_per_transfer = 10\nmax_per_day = 20\n";
        with(ok).unwrap().validate_for(Command::Refill).unwrap();
        let mismatch = format!("{ok}external_destination = \"tdesT\"\n");
        let e = with(&mismatch)
            .unwrap()
            .validate_for(Command::Refill)
            .unwrap_err();
        assert!(e.to_string().contains("does not match"), "{e}");
        let bad_target = ok.replace("target = 5", "target = 1");
        assert!(with(&bad_target)
            .unwrap()
            .validate_for(Command::Refill)
            .is_err());
        let drain_without_reserve = ok.replace("top_up", "drain_above");
        assert!(with(&drain_without_reserve)
            .unwrap()
            .validate_for(Command::Refill)
            .is_err());
        assert!(with(ok).unwrap().validate_for(Command::Sweep).is_err());
    }

    #[test]
    fn exact_float_amounts() {
        let c = with(
            "[refill]\nmode = \"drain_above\"\ndestination = \"t\"\nreserve = 0.1\n\
             fee_headroom = 0.001\nmax_per_transfer = \"2.5\"\nmax_per_day = 3\n",
        )
        .unwrap();
        let r = c.refill.unwrap();
        assert_eq!(r.reserve, Some(Zat(10_000_000)));
        assert_eq!(r.fee_headroom, Zat(100_000));
        assert_eq!(r.max_per_transfer, Zat(250_000_000));
    }

    #[test]
    fn credentials() {
        let c = with("").unwrap();
        assert!(c.rpc.credentials(None).is_err());
        let cred = c.rpc.credentials(Some("pw".into())).unwrap();
        assert_eq!((cred.user.as_str(), cred.password.as_str()), ("pool", "pw"));
        assert!(!format!("{cred:?}").contains("pw"));
        let dir = tempfile::tempdir().unwrap();
        let cookie = dir.path().join(".cookie");
        std::fs::write(&cookie, "__cookie__:secret\n").unwrap();
        let text = BASE.replace(
            "user = \"pool\"",
            &format!("cookie_file = {:?}", cookie.display().to_string()),
        );
        let cred = Config::parse(&text).unwrap().rpc.credentials(None).unwrap();
        assert_eq!(
            (cred.user.as_str(), cred.password.as_str()),
            ("__cookie__", "secret")
        );
    }
}
