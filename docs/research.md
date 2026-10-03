# `src/engine/research.rs`: research windows and start equity

Status (3 October 2026): configured for the top 20 only. Research windows must be deleted and refetched for this configuration; earlier top-50 results do not validate it.

The three fixed, back-to-back 14-day windows of real Bybit data used for strategy selection. `examples/fetch_research_data.rs` stores them in the runtime folder.

| Window | From (UTC) | To (UTC) |
|---|---|---|
| 1 | 2026-08-20 08:30 | 2026-09-03 08:30 |
| 2 | 2026-09-03 08:30 | 2026-09-17 08:30 |
| 3 | 2026-09-17 08:30 | 2026-10-01 08:30 |

| Item | What it does |
|---|---|
| `WINDOW_3_FIRST_BAR` | Open time of window 3's first bar (2026-09-17 08:30 UTC) |
| `window_bounds(k)` | Open times of window `k`'s first and last bar (exactly 1,344 bars) |
| `window_file(dir, k)` | `dir/window_k.db` |
| `load_window(dir, k)` | Window `k` as a `Market` with every stored symbol and its Bybit rules; an error if the file is missing |
| `real_balance()` | The real account's free USDT: wallet minus margin committed elsewhere (Keychain credentials, read-only) |
| `start_equity()` | Research start equity: `EQ=…` if set (to study another size explicitly), otherwise the real balance. There is no built-in default. |

**Why three windows:** one 14-day test can't tell an edge from luck. A strategy is only kept if it holds up in all three windows and in both 14-day forward tests (settings chosen on one window, traded on the next). Each test is still exactly 14 days.

Listing-aware coverage: each symbol's data starts at its Bybit `launchTime` rounded up to a bar, or at Bybit's first candle when trading began later (see `first_trades` in [data.md](data.md)). Scoring requires 110 contiguous candles after that start; listing times are never inferred from cached candles.
