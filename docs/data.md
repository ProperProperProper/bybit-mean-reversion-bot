# `src/engine/data.rs`: Bybit data and the SQLite cache

Everything the bot knows about the market comes through this file, over REST (the live mark-price WebSocket monitor lives in the service, [bot.md](bot.md)). Public endpoints supply instruments, tickers, closed traded and mark-price klines, settled funding, risk limits and order books. Two signed, read-only endpoints supply the account's fee rates and wallet. Every number is parsed strictly: a missing or garbled field drops that row or fails the call, and is never replaced by a default.

## Constants

| Name | Value | Meaning |
|---|---|---|
| `BASE` | `https://api.bybit.com` | Bybit mainnet REST |
| `PACE` | 70 ms | Minimum gap between public requests (~14/s) |

## Types and parsing

- **`LotFilter`:** `qty_step`, `min_qty`, `min_notional`, `max_market_qty` from `lotSizeFilter`.
- **`strict(v)`:** a finite number sent as a string or number, else `None`.
- **`AccountBalance { wallet, committed }`:** the USDT wallet and the part committed to open positions, open orders and locks. `reserved()` is what this bot must never treat as free: `committed`, or the whole wallet when Bybit doesn't report it.
- **`parse_account(coin)` (private):** reads `walletBalance` (required) and `totalPositionIM`, `totalOrderIM`, `locked`. Portfolio-margin accounts return `""` for the IM fields; then `committed` is `None`.
- **`parse_tier(t)` (private):** one risk tier (`riskLimitValue`, `maintenanceMargin`, `maxLeverage`) plus its published `mmDeduction`, which Bybit leaves empty for every lowest tier and for every tier of some symbols.
- **`with_deductions(tiers)` (private):** sorts the tiers and derives each deduction from continuity of maintenance margin at the boundaries: `d[i] = d[i-1] + limit[i-1] × (rate[i] − rate[i-1])`, starting at 0. This reproduces Bybit's published values exactly (BTC, ETH, SOL and DOGE, all tiers, 1 October 2026). A published value that disagrees rejects the symbol.

## `struct Client`

An HTTP client (5 s connect timeout, 15 s total) with a shared pacing lock.

| Function | What it does |
|---|---|
| `get` (private) | Paced public GET. Up to 5 attempts: rate limits and Bybit's transient service error (HTTP 429, `retCode` 10006/10016/10018) back off 2, 4, 8… s; network errors 0.5, 1, 2… s; any other non-zero `retCode` fails at once. |
| `signed_get` (private) | Bybit v5 HMAC-SHA256 signed GET, **re-signed and retried** the same way. Only ever used for reads. |
| `usdt_perpetual_lots` | USDT linear perpetuals that are `Trading`, not scheduled for delisting, not pre-listing, have a finite `leverageFilter.maxLeverage > 1`, and are tokens (no stock, ETF, forex or commodity contracts; `is_token_perpetual`), each with its lot filter and `launchTime`. This is the only source of tradeable symbols: **delisted and delisting tokens are hard-excluded**. |
| `top_margin_tokens(instruments, count)` | The `count` (20 candidates) with the highest 24h turnover among those with a lot filter, best first. An instrument without a ticker or a valid turnover is left out of that bar. |
| `risk_limits` | Every symbol's maintenance-margin tiers, deductions derived as above. |
| `orderbook(symbol)` | 500 levels per side, best first, and the book timestamp. |
| `taker_fees(creds)` | This account's taker fee per linear symbol (signed). |
| `usdt_account(creds)` | The `AccountBalance` (signed). |
| `fetch_rules(creds, lots)` | Complete `Rules` for every symbol with a lot filter, fee, tiers and a measurable book. **Fails if fewer than half the symbols measure**, so an outage can't replace the stored snapshot with an empty one. |
| `klines_since` / `klines_range` / `mark_range` | Closed 15m traded or mark-price candles, oldest first, paged backwards 1,000 at a time. The forming bar is never included; an invalid OHLC row fails the call. |
| `funding_since` / `funding_range` | Settled funding, paged by 200; a malformed record fails the call. |

## `with_held(candidates, eligible, held)`

The symbols to measure rules for: the turnover candidates plus every held symbol that is still eligible. A coin that drops out of the top 20 while held keeps fresh rules (the rules table is a replaced snapshot).

## `universe(candidates, measured, n)`

The first `n` (20) candidates, in turnover order, that have stored rules. A coin without margin tiers, fee, lot filter or a measurable book never enters the universe.

## `struct Cache`

SQLite (WAL) with tables `bars`, `marks`, `funding`, `rules` (JSON), `instruments` (launch times), `first_trades` and `universe`.

| Function | What it does |
|---|---|
| `put_instruments` / `put_universe` | Launch times; the current top-20 universe. |
| `listing_start` (private) | The first possible candle: launch rounded up to a bar, or later when Bybit's history begins later (`first_trades`). |
| `bars_since` / `marks_since` | The first missing candle from `max(earliest, listing start)`: internal gaps and the tail are retried. |
| `note_first_trade(symbol, since, earliest, fetched)` | When a fetch from the listing boundary of a symbol listed **inside** the window returns its first candle later, records that candle as where trading began. |
| `put_bars` / `put_marks` / `put_funding` | Upserts in one transaction. |
| `put_rules` / `rules_symbols` | Atomically replaces the rules snapshot; the symbols that have rules. |
| `retain_symbols(keep)` | Deletes every row (candles, marks, funding, rules, listing data, universe) of symbols not in `keep`. The service calls it every bar with the eligible trading tokens, so **delisted or unleveraged tokens never stay in the data**. |
| `contents` / `last_funding_ts` / `prune` | Symbols and newest bar; newest funding; drop data older than the window. |
| `market(symbols, last_ts)` | A `Market` of exactly 1,344 bars ending at `last_ts`: traded and mark candles on the grid (gaps stay `None`), funding up to the end of the last bar, rules, listing starts and universe membership. |

## `sync(client, cache, symbols, progress)`

Brings every symbol's last 14 days up to date and returns the last fully closed bar. Bars are fetched from the first missing candle (recording a new listing's first trade), then mark candles likewise, then funding from 8 hours before the newest stored settlement. Up to 6 symbols in flight, globally paced. A failing symbol is skipped for the bar with a warning; more than 20% failing is an error. Data older than 16 days is pruned.
