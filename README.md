# Bybit Mean Reversion Bot

A Rust paper-trading and research service for Bybit USDT perpetuals. It reads real exchange data: closed traded and mark-price candles, settled funding, instrument filters, margin tiers, this account's fees and wallet, and current order books. **It never places orders.**

**Status (2 October 2026):** every accounting, data and recovery defect found so far is fixed, with regression tests ([audit](docs/audit.md)). The strategy is now **long only**. Its entry signal has a proven out-of-sample **relative** edge (it picks coins that beat the average coin); the full long-only strategy, which also carries market risk, passes its walk-forward on 1 of 4 real 14-day data sets ([validation](docs/validation.md)). Paper trades only while the walk-forward gate allows. Agents: read [AGENTS.md](AGENTS.md).

## Strategy

- **Universe:** the top 50 token USDT perpetuals on Bybit by 24-hour turnover **that have complete Bybit rules** (margin tiers, account fee, lot filter, measured order book). The top 75 are measured hourly; a coin missing any of that is dropped and the next one by turnover takes its place. Stock, ETF, forex and commodity contracts, delistings and pre-listings are excluded.
- **Long only.** Entry signal `CalmDip`: buy the calmest coins that fell most over the last 24 hours (mean of the volatility and 24h-return percentiles). **It passed a pre-registered out-of-sample test** on four never-seen windows (Jun 25 → Aug 20, every coin that traded then, delisted ones included): the coins it buys beat the average coin by +0.85% per 24h, t = 2.7, with the rank correlation negative in every window ([validation](docs/validation.md)). That is a relative edge; a long-only account still moves with the market.
- **Settings:** chosen every bar by a 14-day walk-forward from 32 combinations: 8 or 24-hour holds, 3 or 5 coins, 1× or 2× leverage, no stop or a 10% stop, and an optional BTC trend filter (enter only while BTC is above its 24-hour average).
- **Balance-aware sizing:** pairs are funded from the free balance, i.e. the wallet minus margin committed anywhere on the account (other positions, orders, locks). Smaller balances use fewer coins; a contract whose lot step would distort its slot is left out at that size. Below **5 USDT free, nothing is opened or added**; exits continue.
- **Execution model:** decided at a 15-minute close, filled at the next open at the measured order-book cost, with this account's taker fee, Bybit lot rounding and tier leverage limits. A rebalance fills all legs or none. Funding is valued at the mark price and drawn from free cash, then isolated margin. Liquidation triggers on mark-price extremes, using real tiers and deductions.

## Validation gate

1,344 closed bars: three rounds of 8 days in-sample and 2 days out-of-sample, then final settings chosen on the latest 8 days. `PASSED` needs a profitable out-of-sample result with profit factor > 1.2, no liquidations, profit without the best window, at least 8 trades, and final settings profitable within a 25% drawdown over the 14 days. `FORWARD TEST` (paper may trade) relaxes only the profit-factor and best-window gates. Otherwise paper opens nothing.

## Run

```sh
./deploy.sh                                   # build, test, install, (re)start the launchd service
cargo run --release --bin bot -- backtest     # one live-equivalent walk-forward (needs credentials)
cargo run --release --example fetch_research_data
EQ=<balance> cargo run --release --example research
cargo run --release --example validate_cached -- SNAPSHOT_DIR <balance> out.json
cargo test --release --all-targets
cargo clippy --all-targets -- -D warnings
```

The console is at `http://127.0.0.1:8787`. Runtime data lives in `~/Library/Application Support/BybitMeanReversionBot`, outside `~/Documents`, which macOS blocks for launchd jobs. Run research tools on `sqlite3 ".backup"` copies, never on the live files.

Credentials come from the macOS Keychain generic password `unified-combo-grid` / `live` (`api_key`, `api_secret`) and are used only for signed read-only GETs. `deploy.sh` re-signs the installed binary with a fixed identifier so the LuLu firewall rule keeps matching after rebuilds.

## Documentation

One page per source file in [docs/](docs/README.md), plus the [audit](docs/audit.md) and [validation](docs/validation.md) reports.
