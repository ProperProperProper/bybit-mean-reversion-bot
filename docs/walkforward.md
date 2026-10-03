# `src/engine/walkforward.rs`: the parameter search and the strategy grids

The module keeps its historical name; since 2026-10-03 it holds no split walk-forward.

## The search

**User rule:** one window of exactly **1,344 closed 15-minute bars (14 days)**, never split into in-sample and out-of-sample parts. `search_full` backtests every combination over the whole window. The best combination that is **usable** challenges the settings paper is using.

**Usable** means the two safety rules plus a minimum activity check, all over the full 14 days:

- no liquidation;
- maximum drawdown ≤ `MAX_DRAWDOWN_PCT` (25%);
- at least `MIN_TRADES` (8) closed trades;
- no execution error.

The best usable combination is picked by `objective`: `return % − 0.5 × max drawdown %`. Because it's chosen on the same 14 days it's scored on, its 14-day result is in-sample, not a forecast. The honest check that doesn't split a window is chronological: choose on one 14-day window, then trade the next, never-seen one. The dashboard's forward-test chart and `examples/research.rs` do exactly that.

## Constants

| Name | Value | Meaning |
|---|---|---|
| `UNIVERSE` | 20 | Top 20 token USDT perpetuals by 24h turnover with complete Bybit rules (user request) |
| `CANDIDATES` | `UNIVERSE` (20) | Coins measured for rules each refresh; a coin without complete rules shrinks the universe |
| `MAX_DRAWDOWN_PCT` | 25 | Safety rule: maximum drawdown over the 14 days |
| `MIN_TRADES` | 8 | Usable settings must actually trade |
| `LIVE_COMBOS` | 2,949,120 | Size of the live grid |

## The live grid: every parameter (`LIVE_COMBOS`, `live_combo(i)`)

User rule (2026-10-03): **test all**. Every strategy parameter varies. Long only is a separate user rule.

| Parameter | Values |
|---|---|
| Signal and direction | all 8 signals, unflipped and flipped; `Return` at 16 and 96 bars: 18 variants |
| Hold | 8, 16, 24, 32, 64, 96, 144, 192 bars (2h–48h) |
| Coins | 1–5 |
| Leverage | 1×, 2×, 3×, 5× |
| Intrabar stop | none, 5, 10, 20% |
| Take-profit | none, 5, 10, 40% |
| Close-based stop | none, 10% |
| Averaging down (`add_pct`) | none, 10% |
| Drawdown breaker | none, 15% |
| Half size after drawdown | none, 10% |
| Volatility sizing | off, on |
| BTC trend filter | off, on |

18 × 8 × 5 × 4 × 4 × 4 × 2⁶ = **2,949,120** combinations. A full 14-day backtest costs about 0.4 ms on 20 coins, so one search takes about 20 minutes under the CPU governor.

`live_combo(i)` decodes combination `i` in mixed radix, so the grid is never held in memory. A test checks the count, that combinations are distinct, and that every listed value occurs.

## Research grids

- `XS_SIGNALS`: the eight ranking signals.
- `long_grid()` / `long_family_grid(signal)`: small long-only grids per signal.
- `xs_grid()` / `xs_family_grid(signal)`: market-neutral grids per signal.

`examples/research.rs` uses these grids to compare signal families.

## `SearchReport` and `search_full(m, scores, equity, deadline, combos, combo)`

`search_full` validates the market (exactly 14 days, complete per-symbol data), backtests combinations `0..combos` built by `combo(i)`, and calls the CPU governor's `checkpoint` between them. The deadline aborts the whole search.

The report holds:

- the window (first and last bar) and the start equity;
- the number of combinations and how many were evaluated;
- how many were usable;
- the elapsed time;
- the best usable settings with their 14-day metrics.

## Champion/challenger (`Choice`, `pick`, `live_score`, `choose_live`)

Paper always trades the best known **usable** settings (user design, 2026-10-03):

- **`live_score(m, scores, equity, p)`:** `objective` over the full 14 days if `p` is usable there; otherwise `None`.
- **`pick(challenger, champion)`:** the challenger (the search's best) replaces the champion (the settings in use) only if it scores **strictly** higher. A tie, an unusable challenger or identical settings keep the champion. An unusable champion is replaced by a usable challenger. If neither is usable, the result is `NoneSafe`.
- **`choose_live`:** re-scores both on the current market and calls `pick`. It returns the settings and a `Choice` (`NewBest`, `KeptLastBest` or `NoneSafe`).
