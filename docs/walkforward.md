# `src/engine/walkforward.rs`: the 14-day walk-forward and the strategy grids

Status (2 October 2026): the gates run on the corrected engine (mark-price liquidation, isolated funding, neutral fills). On real data the live grid currently fails them; see [validation.md](validation.md).

## The test

Exactly **1,344 closed 15-minute bars (14 days)**, split into three rounds:

| Round | In-sample (settings chosen) | Out-of-sample (judged, unseen) |
|---|---|---|
| 1 | bars 0–767 (8 days) | bars 768–959 (2 days) |
| 2 | bars 192–959 | bars 960–1151 |
| 3 | bars 384–1151 | bars 1152–1343 |

Then the **final settings** are chosen on the most recent 8 days. Those are what paper trading uses.

## Constants

| Name | Value | Meaning |
|---|---|---|
| `IS_BARS`, `OOS_BARS`, `WINDOW_STARTS` | 768, 192, [0, 192, 384] | The rounds above |
| `UNIVERSE` | 20 | Top 20 token USDT perpetuals by 24h turnover with complete Bybit rules (user request, 2026-10-03) |
| `CANDIDATES` | `UNIVERSE` (20) | Coins measured for rules each refresh: only the top 20; a coin without complete rules shrinks the universe instead of being replaced |
| `MIN_IS_TRADES`, `MIN_OOS_TRADES` | 8, 8 | Settings must actually trade |
| `MAX_DRAWDOWN_PCT` | 25 | Drawdown cap |
| `MIN_PROFIT_FACTOR` | 1.2 | Out-of-sample profit-factor gate |

## `fn objective(m) -> f64` (private)

Score used to pick settings in-sample: `return % − 0.5 × max drawdown %`. It rewards return but penalises the path.

## `live_grid()`

Long only, `Signal::CalmDip` unflipped (the direction comes from the full-universe signal study, `examples/signal_ic.rs`, not from P&L). **10,000 combinations** (user request, 2026-10-03): hold 8/16/24/32/48/64/96/144/192/288 bars (2h–72h), 1–5 coins, 1×/2×/3×/5× leverage, stop none/5/10/15/20%, take-profit none/5/10/20/40%, BTC trend filter off/on. A test checks the count and that every combination is distinct.

## Grids

- `XS_SIGNALS`: all eight ranking signals.
- `long_grid()` / `long_family_grid(signal)`: long only, both directions, hold 16/32/96, 3 or 5 coins, 1× or 2×, stop none or 10%, regime off or BTC trend (96 per signal, 192 for `Return`).
- `xs_grid()` / `xs_family_grid(signal)`: market-neutral research grids:

| Setting | Options |
|---|---|
| `flip` | both directions |
| `lookback` | 16 or 96 for `Return`; otherwise 0 (unused) |
| `hold` | 16, 32 or 96 bars |
| `top` | 5 or 10 |
| `gross_leverage` | 1 or 2 |
| `stop_pct` | none or 20% |
| `risk` | off |

## `struct WfWindow<P>` / `struct WfReport<P>` / `type XsReport`

One round's bar ranges, chosen settings and in- and out-of-sample metrics. The report adds:

- the combined out-of-sample metrics,
- the final settings with their 8-day and full 14-day self-check,
- `positive_windows` and `net_without_best`,
- the verdict `passed` and the `reasons` it failed.

## Champion/challenger (`Choice`, `pick`, `live_score`, `choose_live`)

Paper always trades the best known **safe** settings (user design, 2026-10-03):

- **`live_score(m, scores, equity, p)`:** `objective` on the latest `IS_BARS` if `p` trades at least `MIN_IS_TRADES` times there without liquidating, **and** it passes the two safety rules over all 14 days (no execution error, no liquidation, drawdown ≤ `MAX_DRAWDOWN_PCT`). `None` means not usable now.
- **`pick(challenger, champion)`:** the challenger (the search's best) replaces the champion (the settings in use) only if it scores **strictly** higher; a tie, an unusable challenger or the same settings keep the champion. An unusable champion is replaced by a usable challenger. If neither is usable: `NoneSafe`.
- **`choose_live`:** re-scores both on the current market and calls `pick`. Returns the settings and a `Choice` (`NewBest`, `KeptLastBest`, `NoneSafe`).

The walk-forward verdict `passed` is information about the selection method; it no longer decides whether paper trades.

## `fn wf_best(grid, range, bt, deadline, evaluated)` (private)

The best settings on `range` by `objective`, among those that:
- run without an execution error and trade at least `MIN_IS_TRADES` times without liquidating in `range`, **and**
- survive all data up to the end of `range` (no liquidation, drawdown ≤ 25%).

It calls the CPU governor's `checkpoint` between candidates.

## `pub fn run_wf(ts, grid, equity, deadline, bt) -> Result<WfReport<P>>`

The generic walk-forward. `bt(params, range)` must backtest from `equity` using only data up to `range.end`.

1. Rejects anything but 1,344 contiguous 15-minute bars.
2. For each round: choose settings in-sample, run them out-of-sample, and add up the results.
3. Choose the final settings on the last 8 days and self-check them over all 14 days.
4. **Gates.** Every failed gate adds a reason, and `passed` means none failed:
   - no round without settings;
   - zero out-of-sample liquidations;
   - net **still positive without the best out-of-sample window**, so one lucky 2-day stretch can't carry a pass;
   - at least 8 out-of-sample trades;
   - out-of-sample profit factor > 1.2;
   - out-of-sample net > 0;
   - final settings liquidation-free, drawdown ≤ 25% and profitable over the 14 days.

## `pub fn run_xs_with(m, scores, equity, deadline, grid) -> Result<XsReport>`

`run_wf` with `xs::backtest` as the backtester. This is what the live service, `bot backtest` and the research tools call.
