# `src/engine/rules.rs`: per-coin Bybit trading rules

Every rule is measured from Bybit ([data.md](data.md), `fetch_rules`); a coin without complete rules is not traded. Interpolating the book curve and the isolated liquidation formula are the model parts.

## `const BOOK_POINTS`

Order sizes (USDT) at which the book cost is measured: 10, 25, 50, 100, 200, 400, 800, 1,600, 3,200, 6,400.

## `struct MarginTier`

`limit` (position value), maintenance-margin `rate`, `deduction` (published by Bybit or derived from tier continuity, see data.md) and `max_leverage`.

## `struct Rules`

| Field | Source |
|---|---|
| `qty_step`, `min_qty`, `min_notional`, `max_market_qty` | `lotSizeFilter` |
| `taker_fee` | `/v5/account/fee-rate` for this account |
| `mm_tiers` | `/v5/market/risk-limit`, ascending |
| `book`, `book_ts` | `/v5/market/orderbook` walked at each `BOOK_POINTS` size: (size, buy cost, sell cost) as a fraction of mid, and when |

Bybit publishes no historical books, so backtests use the latest measurement; the service re-measures hourly.

| Function | What it does |
|---|---|
| `order_qty(qty, price)` | Rounds **down** to the step (capped at the market-order maximum); `None` below the minimum quantity or order value. |
| `order_notional_cap(reference)` | Half the measured depth (and the market-order maximum): the other half is kept as closing liquidity. |
| `book_cost(notional, buy)` | Linear interpolation between measured sizes; the smallest size's cost below it; `None` beyond measured depth or for a malformed curve. |
| `liquidation_price(side, qty, entry, margin, fee_mult)` | Isolated-margin level where margin plus unrealised P&L equals maintenance margin (tier rate, deduction) plus the closing fee, solved in the tier that contains the resulting position value. Mark prices trigger it. |
| `leverage_allowed(value, leverage)` | Within the maximum leverage of the tier that holds `value`. |

## Book measurement

**`walk_book(levels, mid, notional)`** fills a market order level by level and returns `|vwap / mid − 1|`, or `None` if the levels run out. **`measure_book(bids, asks)`** applies it at each `BOOK_POINTS` size on both sides, stopping at the first size either side can't fill; a crossed or unsorted book gives no curve.

## Test-only

`TEST_COST` and `Rules::test_liquid()` (deep uniform book, 0.055% fee, one 1% tier) exist only under `#[cfg(test)]`.
