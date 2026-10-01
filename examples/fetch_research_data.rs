//! Fetches the research data: three back-to-back 14-day windows of CLOSED 15m
//! Bybit traded and mark-price bars + settled funding for every USDT perpetual trading today (never a
//! delisted one), plus each symbol's Bybit rules measured NOW (order rules, the
//! account's taker fee, margin tiers, order-book cost: `rules::Rules`), into
//! the runtime folder (`engine::runtime_dir`) as the three research windows
//! (`engine::research`: Aug 20 -> Sep 3 -> Sep 17 -> Oct 1 2026, 08:30 UTC).
//! Only fetches symbols a window does not have yet. Read-only towards Bybit.
use bybit_mean_reversion_bot::engine::data::{self, Cache, Client};
use bybit_mean_reversion_bot::engine::keychain;
use bybit_mean_reversion_bot::engine::walkforward;
use bybit_mean_reversion_bot::engine::{research, BAR_MS};
use futures_util::{stream, StreamExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let dir = bybit_mean_reversion_bot::engine::runtime_dir();
    let client = Client::new()?;
    let creds = keychain::load()?;
    println!(
        "account USDT wallet balance: {:.2}",
        client.usdt_account(&creds).await?.wallet
    );
    let lots = client.usdt_perpetual_lots().await?;
    let candidates = client
        .top_margin_tokens(&lots, walkforward::CANDIDATES)
        .await?;
    eprintln!("measuring {} order books...", candidates.len());
    let rules = client.fetch_rules(&creds, &candidates).await?;
    let measured = rules.iter().map(|(s, _)| s.clone()).collect();
    let lots = data::universe(&candidates, &measured, walkforward::UNIVERSE);
    let symbols: Vec<String> = lots.iter().map(|(s, _, _)| s.clone()).collect();
    println!(
        "rules for {} of {} candidates; universe {} coins",
        rules.len(),
        candidates.len(),
        symbols.len()
    );
    for k in 1..=3 {
        let (first, last) = research::window_bounds(k);
        let cache = Cache::open(research::window_file(&dir, k).to_str().unwrap_or_default())?;
        cache.put_rules(&rules)?;
        cache.put_instruments(&lots)?;
        cache.put_universe(&symbols)?;
        let (client, cache2) = (&client, &cache);
        let mut jobs = stream::iter(symbols.iter().cloned())
            .map(|s| async move {
                let since = cache2.bars_since(&s, first, last)?;
                if since <= last {
                    let bars = client.klines_range(&s, since, last).await?;
                    cache2.put_bars(&s, &bars)?;
                    cache2.note_first_trade(&s, since, first, &bars)?;
                }
                let marks = cache2.marks_since(&s, first, last)?;
                if marks <= last {
                    cache2.put_marks(&s, &client.mark_range(&s, marks, last).await?)?;
                }
                cache2.put_funding(&s, &client.funding_range(&s, first, last + BAR_MS).await?)?;
                anyhow::ensure!(
                    cache2.bars_since(&s, first, last)? > last,
                    "missing post-listing candle for {s}"
                );
                anyhow::ensure!(
                    cache2.marks_since(&s, first, last)? > last,
                    "missing post-listing mark candle for {s}"
                );
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
        anyhow::ensure!(
            failed == 0,
            "window {k} data incomplete: {failed} fetch failures"
        );
    }
    Ok(())
}
