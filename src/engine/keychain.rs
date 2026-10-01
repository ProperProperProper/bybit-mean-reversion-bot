//! Bybit API credentials from the macOS Keychain (never from files or env vars),
//! used READ-ONLY: to read the account's own fee rates and USDT wallet balance.
//! The credentials are kept in memory only, never written or logged, and this
//! bot never places orders.

use anyhow::{anyhow, Context, Result};
use std::process::Command;

pub const SERVICE: &str = "unified-combo-grid";
pub const ACCOUNT: &str = "live";

pub struct Credentials {
    pub api_key: String,
    pub api_secret: String,
}

/// Reads the JSON `{"api_key": ..., "api_secret": ...}` stored as a generic password.
pub fn load() -> Result<Credentials> {
    let out = Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", SERVICE, "-a", ACCOUNT, "-w"])
        .output()
        .context("running /usr/bin/security")?;
    if !out.status.success() {
        return Err(anyhow!(
            "Keychain item service={SERVICE} account={ACCOUNT} not readable (exit {})",
            out.status
        ));
    }
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("Keychain item is not JSON")?;
    let field = |k: &str| {
        v[k].as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("Keychain JSON missing {k}"))
    };
    Ok(Credentials {
        api_key: field("api_key")?,
        api_secret: field("api_secret")?,
    })
}
