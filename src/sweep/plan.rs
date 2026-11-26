//! Which coinbase addresses are due for a sweep. Pure functions over `listunspent` output.

use std::collections::HashMap;

use crate::amount::Zat;
use crate::config::SweepConfig;
use crate::wallet::{Utxo, COINBASE_MATURITY};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Due {
    /// At least `min_inputs` mature rewards are waiting.
    Inputs,
    /// The oldest mature reward has waited `max_wait_blocks` past maturity.
    Age,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AddressPlan {
    pub address: String,
    pub mature_count: usize,
    pub mature_value: Zat,
    /// Blocks the oldest mature reward has waited since it matured.
    pub oldest_wait_blocks: u64,
    pub due: Option<Due>,
    /// More unswept mature coinbase than `alert_unswept_zec`.
    pub alert: bool,
}

/// Groups mature coinbase by configured address, in configuration order. Outputs that are not
/// coinbase, not mature, or at an address not in the configuration are ignored. Linear in the
/// number of outputs.
pub fn plan(utxos: &[Utxo], cfg: &SweepConfig) -> Vec<AddressPlan> {
    let index: HashMap<&str, usize> = cfg
        .coinbase_addresses
        .iter()
        .enumerate()
        .map(|(i, a)| (a.as_str(), i))
        .collect();
    let mut plans: Vec<AddressPlan> = cfg
        .coinbase_addresses
        .iter()
        .map(|a| AddressPlan {
            address: a.clone(),
            mature_count: 0,
            mature_value: Zat::ZERO,
            oldest_wait_blocks: 0,
            due: None,
            alert: false,
        })
        .collect();
    for u in utxos {
        if !u.generated || u.confirmations < COINBASE_MATURITY {
            continue;
        }
        let Some(&i) = index.get(u.address.as_str()) else {
            continue;
        };
        let p = &mut plans[i];
        p.mature_count += 1;
        p.mature_value = p
            .mature_value
            .checked_add(u.amount)
            .unwrap_or(p.mature_value);
        p.oldest_wait_blocks = p
            .oldest_wait_blocks
            .max(u.confirmations - COINBASE_MATURITY);
    }
    for p in &mut plans {
        p.due = if p.mature_count >= cfg.min_inputs as usize {
            Some(Due::Inputs)
        } else if p.mature_count > 0 && p.oldest_wait_blocks >= u64::from(cfg.max_wait_blocks) {
            Some(Due::Age)
        } else {
            None
        };
        p.alert = p.mature_value > cfg.alert_unswept_zec;
    }
    plans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(min_inputs: u32, max_wait_blocks: u32) -> SweepConfig {
        SweepConfig {
            coinbase_addresses: vec!["tA".into(), "tB".into()],
            min_inputs,
            max_wait_blocks,
            max_inputs: 50,
            max_batches_per_cycle: 10,
            alert_unswept_zec: Zat(10 * 100_000_000),
        }
    }

    fn cb(addr: &str, confirmations: u64) -> Utxo {
        Utxo {
            txid: format!("{addr}{confirmations}"),
            vout: 0,
            address: addr.into(),
            amount: Zat(312_500_000),
            confirmations,
            generated: true,
        }
    }

    #[test]
    fn due_by_min_inputs() {
        let utxos: Vec<Utxo> = (100..103).map(|c| cb("tA", c)).collect();
        let p = plan(&utxos, &cfg(3, 1000));
        assert_eq!(p[0].due, Some(Due::Inputs));
        assert_eq!(p[0].mature_count, 3);
        assert_eq!(p[1].due, None);
        let p = plan(&utxos, &cfg(4, 1000));
        assert_eq!(p[0].due, None);
    }

    #[test]
    fn due_by_max_wait_blocks() {
        let utxos = vec![cb("tA", 100 + 49)];
        assert_eq!(plan(&utxos, &cfg(10, 50))[0].due, None);
        let utxos = vec![cb("tA", 100 + 50)];
        assert_eq!(plan(&utxos, &cfg(10, 50))[0].due, Some(Due::Age));
        assert_eq!(plan(&utxos, &cfg(10, 50))[0].oldest_wait_blocks, 50);
    }

    #[test]
    fn ignores_immature_non_coinbase_and_foreign() {
        let mut not_cb = cb("tA", 500);
        not_cb.generated = false;
        let utxos = vec![cb("tA", 99), not_cb, cb("tOther", 500)];
        let p = plan(&utxos, &cfg(1, 0));
        assert!(p.iter().all(|p| p.mature_count == 0 && p.due.is_none()));
    }

    #[test]
    fn alerts_above_threshold() {
        let utxos: Vec<Utxo> = (100..104).map(|c| cb("tB", c)).collect();
        let p = plan(&utxos, &cfg(50, 1000));
        assert!(p[1].alert, "4 x 3.125 = 12.5 ZEC > 10");
        assert!(!p[0].alert);
    }

    #[test]
    fn large_listings_are_linear() {
        let utxos: Vec<Utxo> = (0..200_000u64)
            .map(|i| cb(if i % 2 == 0 { "tA" } else { "tB" }, 100 + i))
            .collect();
        let started = std::time::Instant::now();
        let p = plan(&utxos, &cfg(10, 100));
        assert_eq!(p[0].mature_count + p[1].mature_count, 200_000);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }
}
