//! Checks a transaction the wallet just built against what the tool asked for. They read the
//! verbose `getrawtransaction` JSON and fail closed: a field they need that is missing, or a shape
//! they do not recognise, is a failure.

use anyhow::{bail, ensure, Context, Result};
use serde_json::Value;

use crate::amount::Zat;

fn array<'v>(tx: &'v Value, key: &str) -> Result<&'v Vec<Value>> {
    tx.get(key)
        .and_then(Value::as_array)
        .with_context(|| format!("transaction has no {key:?} array"))
}

/// The actions of a shielded pool section (`orchard` or `ironwood`); absent means none.
fn actions(tx: &Value, pool: &str) -> Result<usize> {
    match tx.get(pool) {
        None | Some(Value::Null) => Ok(0),
        Some(section) => Ok(section
            .get("actions")
            .and_then(Value::as_array)
            .with_context(|| format!("the {pool} section has no actions array"))?
            .len()),
    }
}

/// The net value leaving a pool section (positive when notes are spent out of it).
fn value_balance_zat(tx: &Value, pool: &str) -> Result<i64> {
    match tx.get(pool) {
        None | Some(Value::Null) => Ok(0),
        Some(section) => section
            .get("valueBalanceZat")
            .and_then(Value::as_i64)
            .with_context(|| format!("the {pool} section has no valueBalanceZat")),
    }
}

fn common(tx: &Value) -> Result<()> {
    let version = tx
        .get("version")
        .and_then(Value::as_u64)
        .context("transaction has no version")?;
    ensure!(
        version >= 6,
        "transaction version {version} cannot carry Ironwood actions"
    );
    ensure!(
        tx.get("ironwood").is_some(),
        "transaction has no ironwood section"
    );
    ensure!(
        array(tx, "vShieldedSpend")?.is_empty(),
        "transaction spends Sapling notes"
    );
    ensure!(
        array(tx, "vShieldedOutput")?.is_empty(),
        "transaction creates Sapling outputs"
    );
    ensure!(
        actions(tx, "ironwood")? > 0,
        "transaction has no Ironwood actions"
    );
    Ok(())
}

/// The most a sweep may pay in fees: a 50-input ZIP 317 sweep pays 0.0026 ZEC.
pub const MAX_SWEEP_FEE: Zat = Zat(1_000_000);

/// A coinbase sweep: transparent inputs in, Ironwood notes out, nothing else. When the wallet
/// reported how much it was shielding, the fee (that value minus what entered Ironwood) must be
/// at most `MAX_SWEEP_FEE`.
pub fn check_sweep(tx: &Value, shielding_value: Option<Zat>) -> Result<()> {
    common(tx)?;
    if let Some(value) = shielding_value {
        let into_ironwood = -value_balance_zat(tx, "ironwood")?;
        // Amounts are below the 21M ZEC supply cap, so they fit i64.
        let fee = (value.0 as i64).saturating_sub(into_ironwood);
        ensure!(
            (0..=MAX_SWEEP_FEE.0 as i64).contains(&fee),
            "sweep of {value} ZEC put {into_ironwood} zatoshis into Ironwood: fee {fee} \
             zatoshis is outside 0..={}",
            MAX_SWEEP_FEE.0
        );
    }
    ensure!(
        array(tx, "vout")?.is_empty(),
        "sweep has transparent outputs"
    );
    ensure!(
        actions(tx, "orchard")? == 0,
        "sweep has legacy Orchard actions"
    );
    ensure!(
        !array(tx, "vin")?.is_empty(),
        "sweep has no transparent inputs"
    );
    Ok(())
}

/// A refill: Ironwood (and possibly legacy Orchard) notes in, `amount` to `destination`.
/// `transparent_destination` is false for a shielded destination, which must receive no
/// transparent output at all.
pub fn check_refill(
    tx: &Value,
    destination: &str,
    transparent_destination: bool,
    amount: Zat,
    max_fee: Zat,
) -> Result<()> {
    common(tx)?;
    // With no transparent inputs, the fee is what leaves the shielded pools minus what the
    // transparent outputs receive.
    let shielded_out = value_balance_zat(tx, "ironwood")? + value_balance_zat(tx, "orchard")?;
    let transparent_out: i64 = array(tx, "vout")?
        .iter()
        .map(|o| {
            o.get("valueZat")
                .and_then(Value::as_i64)
                .unwrap_or(i64::MAX)
        })
        .fold(0i64, |a, b| a.saturating_add(b));
    let fee = shielded_out.saturating_sub(transparent_out);
    ensure!(
        fee >= 0 && fee as u64 <= max_fee.0,
        "refill pays a fee of {fee} zatoshis, above fee_headroom {max_fee} ZEC"
    );
    ensure!(
        array(tx, "vin")?.is_empty(),
        "refill spends transparent inputs"
    );
    ensure!(
        value_balance_zat(tx, "orchard")? >= 0,
        "refill moves value into the legacy Orchard pool"
    );
    let vout = array(tx, "vout")?;
    if !transparent_destination {
        ensure!(
            vout.is_empty(),
            "refill to a shielded address has transparent outputs"
        );
        return Ok(());
    }
    if vout.len() != 1 {
        bail!(
            "refill has {} transparent outputs, expected exactly one",
            vout.len()
        );
    }
    let out = &vout[0];
    let addresses: Vec<&str> = out
        .pointer("/scriptPubKey/addresses")
        .and_then(Value::as_array)
        .context("refill output has no address")?
        .iter()
        .filter_map(Value::as_str)
        .collect();
    ensure!(
        addresses == [destination],
        "refill output pays {addresses:?}, not {destination}"
    );
    let value = match out.get("valueZat").and_then(Value::as_u64) {
        Some(z) => Zat(z),
        None => Zat::from_json(out.get("value").context("refill output has no value")?)?,
    };
    ensure!(
        value == amount,
        "refill output is {value} ZEC, expected {amount} ZEC"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn v6_sweep() -> Value {
        json!({
            "version": 6,
            "vin": [{"txid": "aa", "vout": 0}],
            "vout": [],
            "vShieldedSpend": [], "vShieldedOutput": [],
            "orchard": {"actions": [], "valueBalanceZat": 0},
            "ironwood": {"actions": [{}, {}], "valueBalanceZat": -625000000}
        })
    }

    fn v6_refill(addr: &str, zat: u64) -> Value {
        json!({
            "version": 6, "vin": [],
            "vout": [{"valueZat": zat, "scriptPubKey": {"addresses": [addr]}}],
            "vShieldedSpend": [], "vShieldedOutput": [],
            "orchard": {"actions": [], "valueBalanceZat": 0},
            "ironwood": {"actions": [{}, {}], "valueBalanceZat": zat + 15000}
        })
    }

    #[test]
    fn accepts_v6_ironwood_sweep() {
        check_sweep(&v6_sweep(), None).unwrap();
        check_sweep(&v6_sweep(), Some(Zat(625_260_000))).unwrap();
        assert!(
            check_sweep(&v6_sweep(), Some(Zat(700_000_000))).is_err(),
            "fee too high"
        );
    }

    #[test]
    fn rejects_v5_sapling_sweep() {
        let v5 = json!({
            "version": 5, "vin": [{"txid": "aa"}], "vout": [],
            "vShieldedSpend": [], "vShieldedOutput": [{"cv": "00"}],
            "orchard": {"actions": [], "valueBalanceZat": 0}
        });
        let e = check_sweep(&v5, None).unwrap_err();
        assert!(e.to_string().contains("version 5"), "{e}");
        let mut sapling_out = v6_sweep();
        sapling_out["vShieldedOutput"] = json!([{"cv": "00"}]);
        assert!(check_sweep(&sapling_out, None).is_err());
    }

    #[test]
    fn rejects_stray_transparent_output() {
        let mut tx = v6_sweep();
        tx["vout"] = json!([{"valueZat": 1, "scriptPubKey": {"addresses": ["tmX"]}}]);
        assert!(check_sweep(&tx, None)
            .unwrap_err()
            .to_string()
            .contains("transparent outputs"));
        let mut refill = v6_refill("tmDest", 100);
        refill["vout"]
            .as_array_mut()
            .unwrap()
            .push(json!({"valueZat": 5, "scriptPubKey": {"addresses": ["tmOther"]}}));
        assert!(check_refill(&refill, "tmDest", true, Zat(100), Zat(100_000)).is_err());
    }

    #[test]
    fn rejects_sapling_spend() {
        let mut tx = v6_refill("tmDest", 100);
        tx["vShieldedSpend"] = json!([{"nullifier": "00"}]);
        assert!(check_refill(&tx, "tmDest", true, Zat(100), Zat(100_000))
            .unwrap_err()
            .to_string()
            .contains("Sapling"));
    }

    #[test]
    fn refill_destination_and_amount() {
        let fee = Zat(100_000);
        check_refill(&v6_refill("tmDest", 100), "tmDest", true, Zat(100), fee).unwrap();
        assert!(check_refill(&v6_refill("tmOther", 100), "tmDest", true, Zat(100), fee).is_err());
        assert!(check_refill(&v6_refill("tmDest", 99), "tmDest", true, Zat(100), fee).is_err());
        assert!(check_refill(&v6_refill("tmDest", 100), "uDest", false, Zat(100), fee).is_err());
        let mut shielded = v6_refill("x", 0);
        shielded["vout"] = json!([]);
        check_refill(&shielded, "uDest", false, Zat(100), fee).unwrap();
        // A fee above the headroom fails.
        assert!(check_refill(
            &v6_refill("tmDest", 100),
            "tmDest",
            true,
            Zat(100),
            Zat(14_999)
        )
        .unwrap_err()
        .to_string()
        .contains("fee"));
    }

    #[test]
    fn missing_fields_fail_closed() {
        let mut tx = v6_sweep();
        tx.as_object_mut().unwrap().remove("vShieldedOutput");
        assert!(check_sweep(&tx, None).is_err());
        let mut tx = v6_sweep();
        tx["ironwood"] = json!({"valueBalanceZat": 0});
        assert!(check_sweep(&tx, None).is_err());
    }
}
