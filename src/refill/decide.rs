//! How much one refill sends. Pure arithmetic over balances and the day's total.

use crate::amount::Zat;
use crate::config::{RefillConfig, RefillMode};

pub const SECS_PER_DAY: u64 = 86_400;

/// The UTC day number (days since 1970-01-01) of a Unix time.
pub fn utc_day(unix: u64) -> u64 {
    unix / SECS_PER_DAY
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    Send(Zat),
    Skip(String),
}

/// drain_above: the destination after `last` in config order, wrapping around. When `last` is
/// unset or no longer configured, the first.
pub fn next_in_turn<'a>(destinations: &'a [String], last: Option<&str>) -> &'a str {
    let after = last
        .and_then(|l| destinations.iter().position(|d| d == l))
        .map_or(0, |i| i + 1);
    &destinations[after % destinations.len()]
}

/// top_up: the destination with the lowest balance; the first in config order on a tie. If it
/// is not below low_water, none is, and `decide` skips.
pub fn lowest_balance(balances: &[(String, Zat)]) -> Option<&(String, Zat)> {
    balances
        .iter()
        .reduce(|best, b| if b.1 < best.1 { b } else { best })
}

/// `source_spendable` is the pool address's balance at `minconf`; `dest_balance` is the
/// destination's balance (top_up only); `sent_today` includes any transfer still in flight.
pub fn decide(
    cfg: &RefillConfig,
    source_spendable: Zat,
    dest_balance: Option<Zat>,
    sent_today: Zat,
) -> Decision {
    let want = match cfg.mode {
        RefillMode::TopUp => {
            let low = cfg.low_water.expect("validated: top_up has low_water");
            let target = cfg.target.expect("validated: top_up has target");
            let balance = dest_balance.expect("top_up reads the destination balance");
            if balance >= low {
                return Decision::Skip(format!(
                    "destination holds {balance} ZEC, not below low_water {low} ZEC"
                ));
            }
            target.saturating_sub(balance)
        }
        RefillMode::DrainAbove => {
            let reserve = cfg.reserve.expect("validated: drain_above has reserve");
            if source_spendable <= reserve {
                return Decision::Skip(format!(
                    "source holds {source_spendable} ZEC, not above reserve {reserve} ZEC"
                ));
            }
            source_spendable.saturating_sub(reserve)
        }
    };
    // Never ask for more than the source can pay with the fee on top.
    let affordable = source_spendable.saturating_sub(cfg.fee_headroom);
    let amount = want.min(affordable).min(cfg.max_per_transfer);
    if amount < cfg.min_transfer {
        return Decision::Skip(format!(
            "transfer of {amount} ZEC would be below min_transfer {} ZEC (wanted {want}, \
             spendable {source_spendable})",
            cfg.min_transfer
        ));
    }
    let day_left = cfg.max_per_day.saturating_sub(sent_today);
    if amount > day_left {
        return Decision::Skip(format!(
            "transfer of {amount} ZEC would pass max_per_day {} ZEC ({sent_today} ZEC sent today)",
            cfg.max_per_day
        ));
    }
    Decision::Send(amount)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn zec(s: &str) -> Zat {
        Zat::parse_zec(s).unwrap()
    }

    fn cfg(mode: &str) -> RefillConfig {
        let extra = if mode == "top_up" {
            "low_water = 10\ntarget = 50\n"
        } else {
            "reserve = 5\n"
        };
        Config::parse(&format!(
            "shielded_address = \"u\"\n[rpc]\nurl = \"http://x/\"\nuser = \"u\"\n\
             [refill]\nmode = \"{mode}\"\ndestination = \"t\"\n{extra}\
             max_per_transfer = 30\nmax_per_day = 70\nmin_transfer = 0.01\n"
        ))
        .unwrap()
        .refill
        .unwrap()
    }

    #[test]
    fn top_up_below_and_above_low_water() {
        let c = cfg("top_up");
        assert_eq!(
            decide(&c, zec("100"), Some(zec("8")), Zat::ZERO),
            Decision::Send(zec("30")),
            "wants 42, capped per transfer at 30"
        );
        assert_eq!(
            decide(&c, zec("100"), Some(zec("25")), Zat::ZERO),
            Decision::Skip(
                "destination holds 25.00000000 ZEC, not below low_water 10.00000000 ZEC".into()
            )
        );
        assert!(matches!(
            decide(&c, zec("100"), Some(zec("10")), Zat::ZERO),
            Decision::Skip(_)
        ));
        assert_eq!(
            decide(&c, zec("100"), Some(zec("9.5")), Zat::ZERO),
            Decision::Send(zec("30"))
        );
        assert_eq!(
            decide(&c, zec("100"), Some(zec("29.5")), Zat::ZERO),
            Decision::Skip(
                "destination holds 29.50000000 ZEC, not below low_water 10.00000000 ZEC".into()
            )
        );
    }

    #[test]
    fn fee_headroom_limits_the_amount() {
        let c = cfg("top_up");
        // 20 spendable, default headroom 0.001: sends 19.999.
        assert_eq!(
            decide(&c, zec("20"), Some(zec("0")), Zat::ZERO),
            Decision::Send(zec("19.999"))
        );
        assert!(matches!(
            decide(&c, zec("0.001"), Some(zec("0")), Zat::ZERO),
            Decision::Skip(_)
        ));
    }

    #[test]
    fn drain_above_reserve() {
        let c = cfg("drain_above");
        assert_eq!(
            decide(&c, zec("12"), None, Zat::ZERO),
            Decision::Send(zec("7"))
        );
        assert!(matches!(
            decide(&c, zec("5"), None, Zat::ZERO),
            Decision::Skip(_)
        ));
        // The reserve is far below the headroom here: the headroom still applies.
        let mut c2 = c.clone();
        c2.reserve = Some(Zat::ZERO);
        assert_eq!(
            decide(&c2, zec("3"), None, Zat::ZERO),
            Decision::Send(zec("2.999"))
        );
        assert_eq!(
            decide(&c, zec("100"), None, Zat::ZERO),
            Decision::Send(zec("30")),
            "per-transfer cap"
        );
    }

    #[test]
    fn per_day_cap() {
        let c = cfg("drain_above");
        assert_eq!(
            decide(&c, zec("100"), None, zec("40")),
            Decision::Send(zec("30"))
        );
        assert!(matches!(
            decide(&c, zec("100"), None, zec("41")),
            Decision::Skip(m) if m.contains("max_per_day")
        ));
        assert!(matches!(
            decide(&c, zec("100"), None, zec("70")),
            Decision::Skip(_)
        ));
    }

    fn list(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn drain_above_takes_turns_in_config_order() {
        let d = list(&["a", "b", "c"]);
        assert_eq!(next_in_turn(&d, None), "a");
        assert_eq!(next_in_turn(&d, Some("a")), "b");
        assert_eq!(next_in_turn(&d, Some("b")), "c");
        assert_eq!(next_in_turn(&d, Some("c")), "a", "wraps around");
        assert_eq!(
            next_in_turn(&d, Some("removed")),
            "a",
            "a destination no longer configured starts over"
        );
        let one = list(&["only"]);
        assert_eq!(next_in_turn(&one, None), "only");
        assert_eq!(next_in_turn(&one, Some("only")), "only");
    }

    #[test]
    fn top_up_picks_the_lowest_balance() {
        let b = vec![
            ("a".to_string(), zec("7")),
            ("b".to_string(), zec("3")),
            ("c".to_string(), zec("3")),
            ("d".to_string(), zec("12")),
        ];
        assert_eq!(lowest_balance(&b).unwrap().0, "b", "first of a tie");
        assert_eq!(lowest_balance(&b[..1]).unwrap().0, "a");
        assert_eq!(lowest_balance(&[]), None);
    }

    #[test]
    fn day_rollover() {
        assert_eq!(utc_day(SECS_PER_DAY - 1), 0);
        assert_eq!(utc_day(SECS_PER_DAY), 1);
        assert_eq!(utc_day(1_795_000_000), 20775);
    }
}
