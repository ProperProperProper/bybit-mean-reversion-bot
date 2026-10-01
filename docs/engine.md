# `src/engine/mod.rs` and `src/lib.rs`: core types

Status (2 October 2026): markets carry traded and mark-price candles, listing starts and universe membership, and are validated per symbol. Audit history: [audit.md](audit.md).

`src/lib.rs` only declares the `engine` module. `src/engine/mod.rs` declares the submodules and defines the types every other part uses.

## Constants

| Name | Value | Meaning |
|---|---|---|
| `INTERVAL` | `"15"` | Bybit kline interval in minutes. Locked: every bar is 15 minutes. |
| `BAR_MS` | `900_000` | One bar in milliseconds. |
| `TEST_DAYS` | `14` | Every test uses exactly 14 days. |
| `BARS` | `1344` | `TEST_DAYS × 24 × 4`: the number of 15-minute bars in a test. |

## `pub fn runtime_dir() -> PathBuf`

Returns `~/Library/Application Support/BybitMeanReversionBot`, creating it if needed. Market data, Bybit rules, the research windows, the paper account and logs all live there.

**Why it isn't in the repo folder:** macOS privacy protection blocks launchd background jobs from reading `~/Documents`, so the running service keeps its data in Application Support.

## `struct Bar`

One closed 15-minute candle:

| Field | Meaning |
|---|---|
| `open`, `high`, `low`, `close` | Prices |
| `volume` | Base-asset quantity traded |
| `turnover` | USDT value traded |

## `struct Market`

All symbols on one shared timeline of closed bars:

| Field | Meaning |
|---|---|
| `ts` | Open time of each bar (ms), contiguous 15-minute steps. |
| `symbols` | Symbol names. Their index `s` is used everywhere. |
| `bars[s][t]` | `Some(Bar)`, or `None` when the symbol has no bar at `ts[t]` (not listed yet, or a gap). |
| `marks[s][t]` | Bybit mark-price candle, used for funding valuation and liquidation. Never substituted by traded prices. |
| `listing_times[s]` | Start of the symbol's data (launch or first trade, ms). `None` = unverified, which fails validation. |
| `entry_eligible[s]` | In the current top-50 universe. Held symbols outside it stay managed but are not ranked. |
| `funding[s]` | Real settled funding `(timestamp, rate)`, ascending. |
| `rules[s]` | The symbol's Bybit trading rules (see [rules.md](rules.md)). `None` means the symbol is never traded. |

### `validate()` / `validate_symbol(s)`

`validate` checks the timeline (contiguous 15-minute steps) and row shapes, then every symbol. `validate_symbol` requires every traded and mark candle after the listing start, with sane OHLC (positive prices, high ≥ open/close/low, low ≤ open/close, non-negative volume), and ascending finite funding. The service uses it to let an incomplete symbol sit out a bar.

### `pub fn rules(&self, sym) -> Option<&Rules>`

The symbol's rules, or `None`. Every order path calls this first, so a symbol with unknown rules can't be traded by accident.

### `pub fn concat(parts: &[Market]) -> Result<Market>`

Joins back-to-back markets into one continuous timeline.

1. Checks that each part starts exactly one bar after the previous part ends. Gaps are rejected.
2. Takes the union of symbols. A symbol missing from a part gets `None` bars for that part.
3. Concatenates mark candles the same way and funding without duplicate timestamps; listing starts must agree across parts.
4. Uses the rules from the most recent part that has them, since rules are measured and the newest measurement is best.

**Why:** settings are chosen on one 14-day window and then traded on the next. Joining the windows lets the screener carry its look-back across the edge, as it does live, instead of restarting a warm-up. A test (`concat_is_continuous`) checks that scores on a joined market equal scores on the uninterrupted series.

Timeline length and emptiness are read directly from `Market.ts`.

## `enum Side` and `pub fn sign(self) -> f64`

`Long` or `Short`. `sign()` is `+1.0` for long and `−1.0` for short, so one formula covers both directions. For example, P&L is `sign × (exit − entry) × qty`.
