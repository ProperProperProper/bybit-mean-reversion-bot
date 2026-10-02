# `src/engine/xs.rs`: the strategy engine

A cross-sectional portfolio over the top-50 token USDT perpetuals. At aligned rebalance closes it ranks the universe by a `Signal`, then holds **long the `top` lowest values**, plus, unless `long_only`, **short the `top` highest** (market-neutral), equal notional per leg. The live strategy is long only. The same `step` function drives backtests, the walk-forward and paper trading, so there is no separate "live logic". Audit history: [audit.md](audit.md).

## Constants

| Name | Value | Meaning |
|---|---|---|
| `MIN_ENTRY_BALANCE` | 5 USDT | No rebalance entry and no add while the free balance is below this. Exits continue. |
| `NEUTRAL_TOLERANCE` | 2% | Long and short gross notional may differ by at most this fraction, or the rebalance is not placed |
| `SLOT_HEADROOM` (private) | 0.99 | Share of the free balance a rebalance commits; the rest covers closing the outgoing positions |

## `enum Signal`

| Variant | Value | Unflipped meaning |
|---|---|---|
| `Return` | Return over `lookback` bars | Reversal: long losers, short winners |
| `TrendScore` | Screener trend score | Long the most bearish |
| `PriceAction` | Percentile of signed price action (0 bearish … 100 bullish) | Long the most bearish |
| `Volatility` | Screener volatility score | Long low volatility |
| `Rsi` | RSI(14) | Long oversold |
| `Pulse` | `pulse_long − pulse_short` | Contrarian: long the most bearish |
| `Funding` | Average hourly rate of the last 3 settled fundings | Carry: long negative, short positive |
| `CalmDip` | Mean of the volatility and 24h-return percentiles | **Live:** long the calmest coins that fell most over 24h |

`flip` reverses the direction.

## `struct XsParams`

| Field | Meaning |
|---|---|
| `signal`, `flip` | See above |
| `lookback` | Bars for `Return`; 0 for other signals |
| `hold` | Bars between rebalances (16 = 4h, 32 = 8h, 96 = 24h), aligned to UTC timestamps |
| `top` | Maximum coins per side; smaller balances use fewer (see `funded_targets`) |
| `gross_leverage` | Leverage of each position |
| `stop_pct` | Optional intrabar stop (% against entry) |
| `risk` | Drawdown rules (`Risk`), all off by default |
| `long_only` | Hold only the long side (`slots()` = `top` instead of `2 × top`); the neutrality check is skipped, all legs must still fill |
| `regime` | Entry filter (`Regime`) |

## `enum Regime` and `regime_allows(m, t, r)`

`Off`, or `BtcTrend`: entries only while BTCUSDT's close is above its average close over the last `REGIME_BARS` (96 bars = 24h). When it blocks at a rebalance close, the rebalance holds nothing (existing positions close as usual) and the allocation note says so. Missing BTC history blocks; nothing is assumed.

## `struct Risk`

Each rule is decided on a 15-minute **close** and executed at the **next open**.

| Field | Rule |
|---|---|
| `close_stop_pct` | Exit a position whose close is X% against its entry |
| `short_stop_pct` | The same, for shorts only |
| `take_profit_pct` | Exit a position whose close is X% in its favour |
| `add_pct` | Add the same size once when a close is X% against (slots keep room for it) |
| `breaker_pct` | Close everything when marked equity is X% below the equity at the last rebalance |
| `vol_scaled` | Size by inverse recent volatility, normalised **within each side** so both sides keep equal notional |
| `derisk_pct` | Trade at half size while equity is X% or more below its peak |

## `struct XsPosition`

Symbol index and name, side, entry time, average `entry`, `qty`, posted `margin`, `leverage`, mark-price `liquidation` level, optional `stop`, `fees` and `funding` paid, `last_funding_ts` (so a settlement is never charged twice), last `mark`, queued `next_action` and its `action_not_before`, `added`, and the account's `fee_rate` at entry.

## `struct XsPortfolio`

| Field | Meaning |
|---|---|
| `equity` | Cash: fees, funding and realised P&L applied |
| `reserved` | USDT the real account has committed elsewhere (other positions' and orders' margin, locks); never free here |
| `positions`, `trades` | Open positions and closed trades |
| `pending_targets`, `pending_params`, `pending_slot`, `pending_not_before` | The rebalance decided at a close: targets by symbol name, the parameters and per-slot budget fixed then, and the earliest open it may fill |
| `effective_top`, `allocation_note` | Pairs actually funded at the last decision, and why |
| `execution_error` | Set when a data or execution precondition fails; the simulation stops instead of inventing a fill |
| `rejected_rebalances` | Rebalances not placed because a leg failed or the sides were unbalanced |
| `cost_mult` | Multiplier on measured fees and book costs (1 = as measured) |
| `entries_allowed` | False while the walk-forward fails: no new positions |
| peak / max drawdown, liquidations, breaker state | Running figures |

## Selection

**`signal_value(m, scores, s, t, p)`:** the ranking value, negated if `flip`; `None` when the symbol is outside the screener universe or the value is unavailable.

**`targets(m, scores, t, p)`:** the `top` lowest values long and the `top` highest short, ties broken by index.

**`funded_targets` (private):** the decision used by the portfolio. Starting at `p.top` per side and stepping down to 1, each slot gets `free × scale × 0.99 / slots / (1 + adds)`. A symbol is rankable at that slot only if its measured book can fill the order, Bybit's lot rules accept it, **rounding to the quantity step keeps at least 99% of the notional**, and the leverage is allowed by its tier. The first basket size where every slot fills wins. Returns nothing when the free balance is below `MIN_ENTRY_BALANCE`.

## Orders and accounting

- **`open`:** a market order at the open, sized so margin + entry fee = the slot budget, filled at open × (1 ± measured book cost), quantity rounded down to Bybit's step. Not placed if the symbol has no rules or mark candle, the book is too thin, the minimums or tier leverage fail, or the free balance can't cover it.
- **`add`:** the position's quantity again, same checks, refused while the free balance is below `MIN_ENTRY_BALANCE`.
- **`close` / `close_market` / `exit_price`:** a market close at the measured book cost for its size. A close outside measured depth sets `execution_error`.
- **`charge_funding`:** `side × rate × qty × mark open`. A debit takes free cash first, then the position's isolated margin; the liquidation price is recomputed.
- **`liquidate`:** closes at the liquidation level; loss is capped at the posted margin plus fees and funding.
- **`available()`:** `equity − reserved − Σ margin`.

## `step(m, scores, t, p)`: one closed bar

1. Every held position needs a mark candle at `t`, else `execution_error`.
2. Funding stamped at this open is charged; then any position whose mark open is through its liquidation level is liquidated, **before** any queued action.
3. Queued actions: breaker flatten, then per-position stop / take-profit / rebalance exit; an add only when no rebalance coincides and entries are allowed.
4. Rebalance (if decided earlier and due): waits a bar if any target lacks data; otherwise closes every position, then opens all legs with the fixed slot budget on a copy. The copy replaces the account only if **every** leg filled and (unless long only) the sides are within `NEUTRAL_TOLERANCE`; otherwise `rejected_rebalances` += 1. No entries while the free balance is below the minimum.
5. Inside the bar: intrabar funding is refused (it needs finer mark data); mark-price liquidation (open or adverse extreme) first, then the traded-price stop; then the close-based rules queue the next action.
6. Funding stamped at this bar's close is charged at the next bar's mark open. On the newest loaded bar it waits for that bar.
7. Breaker check, then `decide_close`, then peak and drawdown.

**`decide_close`** fixes targets, parameters and slot budget at aligned closes. **`install_entry_gate`** cancels queued entries (and adds, when disabled) when settings or eligibility change. **`defer_new_decisions`** makes a live decision fill no earlier than the open after it was ready. **`validate_state`** rejects corrupt persisted state. **`close_all`** closes everything at the last traded close with book cost and fee.

## Daily rows and backtests

**`run_daily`** appends one `DayRow` per UTC day (equity, P&L, trades, wins); the console's forward-test chart uses it. **`backtest` / `backtest_costs`** step a fresh account over a range and close everything at the end, so net and profit factor come from the same closed trades.
