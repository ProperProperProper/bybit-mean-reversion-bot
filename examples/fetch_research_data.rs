//! Fetches the research data: three back-to-back 14-day windows of CLOSED 15m
//! Bybit bars + settled funding for every USDT perpetual trading today (never a
//! delisted one), plus each symbol's Bybit rules measured NOW (order rules, the
//! account's taker fee, margin tiers, order-book cost: `rules::Rules`), into
//! the runtime folder (`engine::runtime_dir`) as window_{1,2,3}.db:
//!   window 1: 2026-08-20 08:30 -> 09-03 08:30 UTC
//!   window 2: 2026-09-03 08:30 -> 09-17 08:30 UTC
//!   window 3: 2026-09-17 08:30 -> 10-01 08:30 UTC
//! Only fetches symbols a window does not have yet. Read-only towards Bybit.
use bybit_mean_reversion_bot::engine::data::{Cache, Client};
use bybit_mean_reversion_bot::engine::keychain;
use bybit_mean_reversion_bot::engine::{BARS, BAR_MS};
use futures_util::{stream, StreamExt};

/// First 15m bar of window 3.
pub const WINDOW_3_FIRST_BAR: i64 = 1_789_633_800_000;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = bybit_mean_reversion_bot::engine::runtime_dir();
    let client = Client::new()?;
    let creds = keychain::load()?;
    println!(
        "account USDT wallet balance: {:.2}",
        client.usdt_wallet_balance(&creds).await?
    );
    let lots = client.usdt_perpetual_lots().await?;
    let symbols: Vec<String> = lots.iter().map(|(s, _)| s.clone()).collect();
    eprintln!("measuring {} order books...", symbols.len());
    let rules = client.fetch_rules(&creds, &lots).await?;
    println!("rules for {} of {} symbols", rules.len(), symbols.len());
    let span = BARS as i64 * BAR_MS;
    for k in 1..=3i64 {
        let last = WINDOW_3_FIRST_BAR + span - BAR_MS - (3 - k) * span;
        let first = last - span + BAR_MS;
        let cache = Cache::open(dir.join(format!("window_{k}.db")).to_str().unwrap())?;
        cache.put_rules(&rules)?;
        let (client, cache2) = (&client, &cache);
        let mut jobs = stream::iter(symbols.iter().cloned())
            .map(|s| async move {
                if cache2.last_bar_ts(&s)?.is_none() {
                    cache2.put_bars(&s, &client.klines_range(&s, first, last).await?)?;
                    cache2
                        .put_funding(&s, &client.funding_range(&s, first, last + BAR_MS).await?)?;
                }
                anyhow::Ok(())
            })
            .buffer_unordered(6);
        let (mut done, mut failed) = (0, 0);
        while let Some(r) = jobs.next().await {
            done += 1;
            if let Err(e) = r {
                failed += 1;
                eprintln!("  {e:#}");
            }
        }
        println!("window {k}: {done} symbols checked, {failed} failed");
    }
    Ok(())
}
