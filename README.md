# Bybit Mean Reversion Bot

An **open-source crypto trading bot in Rust** for **Bybit USDT perpetual futures**. It runs a **market-neutral, cross-sectional mean-reversion strategy**, and it was selected only after **walk-forward backtesting** on real exchange data with **no lookahead and no repainting**. It includes a live **paper-trading** service with a web console.

**Paper trading and signals only. It never places orders.**

Topics: algorithmic trading · quant strategy · crypto futures · perpetual swaps · backtesting · walk-forward optimisation · statistical arbitrage · long/short · Rust

## The strategy

On every 15-minute close, the bot scores the **top 100 Bybit USDT perpetuals by 24h turnover** with a screener built from closed bars. The screener measures volatility, price action, volume, an activity rank, a trend score and **Pulse**: volume surge × volatility expansion × short-term direction.

- **Contrarian Pulse.** The bot goes **long the 5 coins with the most bearish Pulse** and **short the 5 with the most bullish**, with equal size on each side, so it is market-neutral. It then rebalances on a fixed UTC schedule. Extreme moves on these coins tend to snap back; the strategy trades that snap-back.
- **Settings re-chosen every bar.** A 14-day walk-forward re-picks the settings (holding time, number of coins, leverage up to 2x, stop) on every bar. New positions open only while it passes.
- **Sized from the real account balance** (read-only), including deposits and withdrawals.
- **Drawdown handling is tested, not guessed.** Nine rules were compared (see *Results*); none improved on "no rule".

## Real trading conditions in every backtest

Backtests use Bybit's own data and rules, never assumed numbers:

- **Closed 15m bars only.** Decisions are made on a bar's close and filled at the next open; stops and liquidations are checked inside the bar, worst case first. Tests prove that no decision changes when future bars change.
- **Order-book slippage.** Every order walks Bybit's real order book (500 levels) for its exact size. Bybit publishes no historical order books, so backtests use the latest measurement, which the live service refreshes hourly.
- **Fees** are your account's own taker fee per coin (read-only `/v5/account/fee-rate`).
- **Margin** uses Bybit's real maintenance-margin tiers per coin, with isolated-margin liquidation.
- **Funding** uses real settled funding rates.
- **Order rules:** Bybit's lot rules (qty step, minimum qty, minimum order value). A coin without rules isn't traded, and an order the account can't fund is rejected, as on Bybit.
- **Universe:** only coins trading today, never delisted ones.

## How it was tested

Every test uses exactly **14 days** of data.

- **Walk-forward (also run live, on every bar):** choose settings on 8 days, trade the next 2 unseen days, three times within the 14 days. It shows **PASSED** only if all of these hold:
  - no liquidations
  - profit factor above 1.2 and net profit positive
  - the net *stays* positive without the single best window
  - the final settings survive the 14 days with drawdown at or below 25%

  If only the profit-factor or best-window check fails, it shows **FORWARD TEST** and paper keeps trading, labelled as not validated. Otherwise it shows **FAILED** and opens nothing new.
- **Strategy selection (`examples/research.rs`):** seven candidate signals ran the walk-forward on three separate 14-day windows (Aug 20 – Oct 1, 2026). Then came two 14-day forward tests, where settings chosen on one window traded the next window, unseen. A single lucky 14-day period can't select a strategy.

## Results

These results use real Bybit data and rules, starting from a **109.48 USDT** account.

**Contrarian Pulse was the only signal positive in all five out-of-sample checks:**

| Check | Result |
|---|---|
| Walk-forward, unseen days (3 windows) | +12.6% / +12.9% / +4.5% |
| 14-day forward test 1 (Sep 3–17) | **+40.6%** (+25.5% at 2x costs), max drawdown 28.0% |
| 14-day forward test 2 (Sep 17–Oct 1) | **+28.3%** (+23.3% at 2x costs), max drawdown 21.3% |
| Liquidations | 0 |

The other signals all lost somewhere: return reversal and momentum, trend score, price action, volatility, RSI and funding carry. The edge is real but thin and lumpy: profit factor is about 1.2–1.35, and gains often come from a few days.

**Drawdown rules** (`examples/risk_variants.rs`) were tested one at a time on the same five checks:

| Rule | Outcome |
|---|---|
| **No rule (live)** | Positive in all five, also at 2x costs |
| Add once at 10% against (fully funded) | Drawdown halved to about 14%, but lost at 2x costs in one forward test |
| Close-based stops (10%, 15%), short-only squeeze stop, take-profit 10% | Lost money or cut returns. They exit the extreme coins right before the snap-back |
| Portfolio breaker, volatility-scaled sizing, half size after a 10% drawdown | Mixed: worse in at least one check |
| 1x leverage | Roughly half the drawdown (11–15%) and half the return |

Nothing here is financial advice. Past results do not predict future ones.

## Run

```sh
cargo run --release --bin bot -- serve                  # paper service + console, http://127.0.0.1:8787
cargo run --release --bin bot -- backtest               # the live strategy's 14-day walk-forward now
cargo run --release --example fetch_research_data       # three 14-day research windows + Bybit rules
cargo run --release --example research                  # signal research
cargo run --release --example risk_variants             # drawdown rules compared
cargo test --release
```

The bot reads your balance and fee rates **read-only**, with Bybit API credentials stored in the macOS Keychain (`security` generic password, service `unified-combo-grid`, account `live`, JSON `{"api_key","api_secret"}`). Credentials are never written to files.

`deploy.sh` builds and tests the bot, installs it to `~/Library/Application Support/BybitMeanReversionBot`, and runs it under launchd on macOS. It keeps running and restarts on errors, panics and hangs, with CPU capped at 85%.

## Code map

| Path | What it is |
|---|---|
| `src/engine/data.rs` | Bybit REST client (bars, funding, instruments, risk limits, order books, fee rates, balance) and the SQLite cache |
| `src/engine/rules.rs` | Per-coin trading rules and the order-book cost model |
| `src/engine/scores.rs` | The screener (causal, closed bars) |
| `src/engine/xs.rs` | Strategy engine: ranking, positions, costs, margin, liquidation, drawdown rules, daily P&L |
| `src/engine/walkforward.rs` | 14-day walk-forward gate and the strategy grids |
| `src/bin/bot/` | The paper-trading service and web console |
| `examples/` | Research tools (the results above) |
