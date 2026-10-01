//! The three fixed, back-to-back 14-day research windows of real Bybit data
//! (stored by `examples/fetch_research_data.rs` as window_{1,2,3}.db in the
//! runtime folder) and the start equity research runs use:
//!   window 1: 2026-08-20 08:30 -> 09-03 08:30 UTC
//!   window 2: 2026-09-03 08:30 -> 09-17 08:30 UTC
//!   window 3: 2026-09-17 08:30 -> 10-01 08:30 UTC

use super::data::{Cache, Client};
use super::{keychain, Market, BARS, BAR_MS};
use anyhow::Result;
use std::path::Path;

/// Open time of window 3's first 15m bar (2026-09-17 08:30 UTC).
pub const WINDOW_3_FIRST_BAR: i64 = 1_789_633_800_000;

/// Open time of the first and last 15m bar of window `k` (1, 2 or 3).
pub fn window_bounds(k: i64) -> (i64, i64) {
    let span = BARS as i64 * BAR_MS;
    let last = WINDOW_3_FIRST_BAR + span - BAR_MS - (3 - k) * span;
    (last - span + BAR_MS, last)
}

/// The cache file of window `k`.
pub fn window_file(dir: &Path, k: i64) -> std::path::PathBuf {
    dir.join(format!("window_{k}.db"))
}

/// Window `k` as a `Market` (every symbol it holds, with its Bybit rules).
pub fn load_window(dir: &Path, k: i64) -> Result<Market> {
    let path = window_file(dir, k);
    anyhow::ensure!(
        path.exists(),
        "research window {} not found: run fetch_research_data",
        path.display()
    );
    let cache = Cache::open(path.to_str().unwrap_or_default())?;
    let (symbols, _) = cache.contents()?;
    cache.market(&symbols, window_bounds(k).1)
}

/// The real account's free USDT: wallet minus margin committed to positions,
/// orders and locks (read-only, Keychain credentials).
pub async fn real_balance() -> Result<f64> {
    let creds = keychain::load()?;
    let account = Client::new()?.usdt_account(&creds).await?;
    Ok(account.wallet - account.reserved())
}

/// Start equity for research: EQ=... if set (to study another size explicitly),
/// otherwise the real account balance. Never a built-in default.
pub fn start_equity() -> Result<f64> {
    if let Ok(v) = std::env::var("EQ") {
        return Ok(v.parse()?);
    }
    tokio::runtime::Runtime::new()?.block_on(real_balance())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_are_back_to_back_and_14_days() {
        for k in 1..=3 {
            let (first, last) = window_bounds(k);
            assert_eq!(last - first, (BARS as i64 - 1) * BAR_MS);
        }
        assert_eq!(window_bounds(2).0 - window_bounds(1).1, BAR_MS);
        assert_eq!(window_bounds(3).0, WINDOW_3_FIRST_BAR);
    }
}
