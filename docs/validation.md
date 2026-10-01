# Validation — 2 October 2026

All numbers come from real Bybit data: closed 15-minute traded and mark-price candles, settled funding, current order books, risk tiers, lot filters and this account's fee rates. They were run on disposable copies of the service's databases, from the account's observed balance (about **110 USDT**).

## Tests

```sh
cargo test --release --all-targets   # 58 tests, all pass
cargo clippy --all-targets -- -D warnings   # clean
```

## Walk-forward of the live grid (Pulse family)

`cargo run --release --example validate_cached -- SNAPSHOT_DIR <balance> out.json`

| Data | Symbols | Verdict | OOS return | OOS profit factor | OOS trades | OOS liquidations | Rejected rebalances |
|---|---|---|---|---|---|---|---|
| Latest 14 days (to 2026-10-01 14:30 UTC) | 50 | fail | −0.55% | 0.99 | 160 | 2 | 0 |
| Window 1 (Aug 20 → Sep 3) | 48 | fail | −3.93% | 0.88 | 140 | 0 | 0 |
| Window 2 (Sep 3 → Sep 17) | 49 | fail | −4.30% | 0.73 | 80 | 1 | 0 |
| Window 3 (Sep 17 → Oct 1) | 49 | fail | −1.74% | 0.96 | 100 | 1 | 0 |

Chronological forward tests (settings chosen on one window, traded on the next, never seen):

| Forward | Return | At 2× costs | Profit factor | Max drawdown |
|---|---|---|---|---|
| Window 1 → 2 | −20.7% | −32.3% | 0.80 | 28.1% |
| Window 2 → 3 | +5.1% | +28.0% | 1.05 | 13.1% |

The second forward's "better at double cost" result is selection noise, not a cost effect. Doubling costs changes order sizes slightly, which changes which coins pass the lot checks at 11–22 USDT slots. In this run that avoided an ARK short liquidation and picked a QNT long that made 18.8 USDT.

**Effect of the rounding fix:** before it, the same grid on the same data rejected 919 rebalances in 192 runs (the account sat flat), and window 2 "passed". After it, every rebalance executes and no window passes.

## Liquidations are real

Every out-of-sample liquidation was checked against the cached mark prices. Examples from the latest 14 days: LONGXIA short, mark +95% (1×) and +47% (2×); QNT short, +100% (1×) and +58% (2×); ARK short, +53%; MOVR short, +49%; SOON short, +52%. The strategy shorts the coins with the strongest bullish Pulse, and on real data some keep squeezing.

## Signal families (`examples/research.rs`, `EQ=<balance>`)

No family passes the walk-forward in more than one of three windows, and none has two positive chronological forward tests:

| Family | Walk-forwards passed | Forward W1→W2 | Forward W2→W3 |
|---|---|---|---|
| Return | 0 / 3 | −6.4% | −15.0% |
| TrendScore | 0 / 3 | −2.5% | +2.7% |
| PriceAction | 0 / 3 | −17.2% | +18.5% |
| Volatility | 1 / 3 | −9.1% | +7.8% |
| Rsi | 1 / 3 | +0.2% | −7.1% |
| Pulse (live) | 0 / 3 | −20.7% | +5.1% |
| Funding | 0 / 3 | −4.2% | −2.5% |
| All together | 0 / 3 | −9.1% | +5.1% |

Switching to the best-looking family on these same windows would be curve-fitting, so the live family is unchanged. The gate keeps paper flat while it fails.

## Live service

Deployed 2026-10-01 14:40 UTC from this tree. The first bars under the new code processed all 50 symbols; the walk-forward reports `FAILED`, so the paper account takes no new positions.

## Reproduce

1. Copy `data.db` and `window_1.db` to `window_3.db` from `~/Library/Application Support/BybitMeanReversionBot` with `sqlite3 SRC ".backup DEST"` (never run tools against the live files).
2. Run `validate_cached` on the copy with the observed balance.
3. Run `EQ=<balance> cargo run --release --example research` for the family table.
