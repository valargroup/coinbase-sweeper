//! Startup refusals shared by both commands.

use anyhow::anyhow;

use crate::runner::{Ctx, StartupError};

/// The pool's shielded address must be a unified address with an Orchard receiver (which holds
/// Ironwood notes after NU6.3), no Sapling receiver, and owned by this wallet.
pub fn check_shielded_address(ctx: &Ctx) -> Result<(), StartupError> {
    let ua = &ctx.cfg.shielded_address;
    let wallet = ctx.wallet();
    let receivers = wallet.unified_receivers(ua).map_err(|e| match e {
        e if e.is_connectivity() => StartupError::from(e),
        e => StartupError::Refuse(anyhow!(
            "shielded_address {ua} is not a unified address this wallet can read: {e}"
        )),
    })?;
    if receivers.iter().any(|r| r == "sapling") {
        return Err(StartupError::Refuse(anyhow!(
            "shielded_address {ua} has a Sapling receiver; create an Orchard-only address with \
             `z_getaddressforaccount <account> [\"orchard\"]`"
        )));
    }
    if !receivers.iter().any(|r| r == "orchard") {
        return Err(StartupError::Refuse(anyhow!(
            "shielded_address {ua} has no Orchard receiver (receivers: {receivers:?})"
        )));
    }
    if !wallet.is_mine(ua, true)? {
        return Err(StartupError::Refuse(anyhow!(
            "shielded_address {ua} does not belong to this wallet"
        )));
    }
    Ok(())
}

/// Whether `address` is a transparent address (`validateaddress` accepts only those).
pub fn is_transparent(ctx: &Ctx, address: &str) -> Result<bool, StartupError> {
    let v = ctx
        .rpc
        .read("validateaddress", serde_json::json!([address]))?;
    Ok(v.get("isvalid").and_then(|b| b.as_bool()) == Some(true))
}
