# `src/engine/scores.rs`: the screener

Status (2 October 2026): `price_action_score` is the percentile of signed price action, and the `PriceAction` signal ranks it. `pa_strength` (unsigned) only feeds the activity rank. `return_score` is the percentile of the 24h return (used by `CalmDip`). `compute_with(m, universe, false)` ranks every symbol that traded at the time, not only today's tradeable ones, for survivorship-free research.

Scores every symbol on every closed 15-minute bar. **Everything at bar `t` uses bars ≤ `t` only**, and cross-sectional parts compare a symbol with the other symbols at the same bar. Tests check that scores don't change when later bars are removed or altered.

## Constants

| Name | Value | Meaning |
|---|---|---|
| `WINDOWS` | 1, 2, 4, 16, 96 bars | 15m, 30m, 1h, 4h and 24h ranges for volatility |
| `WINDOW_WEIGHTS` | 5:4:3:2:1 | Recent intervals weigh more |
| `EMAS` / `EMA_WEIGHTS` | 8/21/55 bars, 3:2:1 | Moving averages for price action |
| `VOLUME_LOOKBACK` | 20 bars | Baseline for the volume Z-score |
| `WARMUP` | 110 bars | Contiguous history needed before a symbol is scored |

## `struct Score`

| Field | Meaning |
|---|---|
| `volatility_score`, `volume_score`, `pa_strength` | Percentiles (0–100) within the universe at this bar |
| `rank_value` / `rank` | Mean of those three percentiles, and the position by it (1 = highest): the **activity rank** |
| `price_action_score` | Percentile of the signed price action (0 bearish … 100 bullish) |
| `trend_score` | `50 + (price_action_score − 50) × rank_value / 100`, clamped to 1–100. It is strongly directional only when the coin is also highly active. |
| `rsi14` | Wilder RSI(14) |
| `turnover_24h` | USDT traded over the last 96 bars |
| `pulse_long`, `pulse_short` | The **Pulse** scores (below). `None` until there is enough 4h history. |

## `struct Raw` and `fn raw_features(bars) -> Vec<Option<Raw>>`

One pass over a symbol's bars computes its own features at every bar (`None` until `WARMUP` contiguous bars exist; a gap restarts the count):

- **Volatility:** for each window, `(highest high − lowest low) / close k bars ago × 100 / √k`, weighted 5:4:3:2:1. Dividing by √k makes windows of different lengths comparable.
- **Price action (signed):** the weighted distance of the close from EMA 8/21/55 in %, divided by volatility. A calm coin's 1% move then counts for more than a wild coin's.
- **Volume Z-score:** this bar's volume against the mean and standard deviation of the previous 20 bars (0 if those 20 bars have zero variance).
- **RSI(14):** Wilder smoothing of gains and losses.
- **24h turnover:** the sum of the last 96 bars' turnover.
- **Pulse:** see `pulse_scores`.

## `fn bbw(closes) -> Option<f64>`

Bollinger band width: `4 × population standard deviation / mean`. `None` if the mean isn't positive.

## `fn sigmoid100(x) -> f64`

`100 / (1 + e^(−x))`. Maps any value to 0–100.

## `fn pulse_scores(bars, t, run_start, vol_z) -> Option<(long, short)>`

The **Pulse** score at bar `t`:

- **Volume surge:** `Z_v`, this bar's volume Z-score.
- **Volatility expansion:** `V_R = BBW(last 20 closes, 15m) / BBW(20 closes taken every 16 bars, i.e. 4h)`. It needs 20 × 4h of contiguous history.
- **Direction:** the 5-bar least-squares slope of the close, as % of price per bar.
  - Long: `D_M = 1 + slope` if the slope is up, else 0.
  - Short: `D_M = 1 + |slope|` if the slope is down, else 0.
- **Score:** `sigmoid100(Z_v × V_R × D_M)` for each side.

A high `pulse_long` means a volume surge with expanding volatility on a rising price. The live strategy uses `pulse_long − pulse_short` as its ranking value and trades it **contrarian** (see [xs.md](xs.md)).

## `fn percentile(sorted, x) -> f64`

The share of values ≤ `x` in an already-sorted list, scaled to 0–100 (50 when there's only one value).

## `pub fn compute(m: &Market, universe: usize) -> Vec<Vec<Option<Score>>>`

Scores as `[symbol][bar]`. At every bar:

1. Collect symbols with raw features at that bar, and keep the **top `universe` by 24h turnover** (100 live). The universe is re-chosen every bar from data up to that bar.
2. Compute the percentiles, rank value, price-action percentile and trend score within that universe.
3. Sort by rank value to assign `rank`.

Symbols outside the universe, or still warming up, get `None`.

Listing-aware coverage: each symbol's data starts at its Bybit `launchTime` rounded up to a bar, or at Bybit's first candle when trading began later (see `first_trades` in [data.md](data.md)). Scoring requires 110 contiguous candles after that start; listing times are never inferred from cached candles.
