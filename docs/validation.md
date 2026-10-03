# Validation — 2 October 2026

All figures below are records of past experiments. The databases they were computed from were deleted in the 2026-10-02 fresh-data reset (user rule: every change restarts the bot on freshly fetched data); rerun the tools to reproduce on new data. The CalmDip holdout is spent and must not be reused for tuning.

## Long-only strategy (current live)

The user moved the bot to long only and asked for better entry signals. Both sections below use real Bybit data and an explicitly supplied observed balance. The market-neutral results further down are kept for reference.

### Entry evidence on the full historical universe (`signal_ic`)

The research windows hold only today's top-50 coins. Today's top-turnover coins are often the ones that just rallied, so long-only results on them are inflated (window 3: holding every coin equally made +47.1%, BTC +9.1%). The entry study therefore uses the pre-refetch windows, which hold candles for every perpetual that traded then (741–782 coins). The universe is the top 50 by turnover **at each bar**, with non-overlapping holds.

| Signal (lowest bought) | IC, 4h hold (W1/W2/W3) | IC, 8h hold | IC, 24h hold | 5 bought vs average coin, 24h hold |
|---|---|---|---|---|
| Volatility | −0.077 / −0.071 / −0.039 | −0.120 / −0.100 / −0.061 | −0.123 / −0.217 / −0.132 | +0.38% / +0.58% / −0.44% |
| 24h return (reversal) | −0.042 / −0.027 / −0.049 | −0.038 / −0.020 / −0.064 | −0.049 / −0.058 / −0.038 | −0.72% / −0.74% / −1.37% |
| **CalmDip** (both) | −0.072 / −0.057 / −0.050 | −0.089 / −0.076 / −0.076 | −0.093 / −0.183 / −0.107 | **+1.03% / +0.24% / +0.23%** |
| Pulse, buying the highest | — | — | — | −2.22% / −1.37% / −1.06% |

A negative IC means lower values did better, which is the direction CalmDip buys. CalmDip's IC is negative in all 9 cells, with t-stats from −1.9 to −4.1. It was designed after seeing these windows, so only new data can confirm it. Pulse momentum, which looks good on today's top 50, lost to the average coin in every window on the full universe.

### Holdout test of CalmDip (pre-registered 2026-10-02, before any holdout data was fetched)

CalmDip was designed on the three research windows, so they cannot prove it. The proof attempt uses earlier 14-day windows nobody has looked at: `window_bounds(k)` for k = −3 … 0 (2026-07-09 → 2026-08-20 08:30 UTC, four windows). The universe is every USDT perpetual token that was trading then, **including since-delisted ones** (`instruments-info` status `Trading` and `Closed`), ranked to the top 50 by turnover at each bar. The signal is exactly as defined in this commit (unflipped, the 5 lowest bought). Nothing is tuned afterwards.

**Pass criteria (all required):**
1. The IC (rank correlation with next-open-to-close return relative to the average coin) is negative in **every** holdout window at both the 8h and 24h holds.
2. The pooled IC t-stat across all holdout samples is ≤ −2 at both holds.
3. Pooled over all holdout windows, the 5 coins bought beat the average coin by more than 0.2% per trade at the 24h hold (an estimate of round-trip taker fees plus book cost).

If any criterion fails, CalmDip is **not** a proven edge, and this document will say so.

**Correction to the registration text:** windows k = −3 … 0 span **2026-06-25 → 2026-08-20** (four 14-day windows), not 07-09 → 08-20. The registered window indices and count were used unchanged.

### Holdout result: **PASS** (all three criteria)

Data (original run): every USDT perpetual token alive in each window, delisted ones included (831 ever listed; 517–527 alive per window), all with candles, 0 fetch failures. The fetcher now hard-excludes delisted tokens (see the rerun below). Study: `SIGNAL=CalmDip WINDOWS=-3,-2,-1,0 signal_ic <copy of holdout/>`; only CalmDip was evaluated.

| Hold | IC per window (W−3 / W−2 / W−1 / W0) | Pooled IC (t) | 5 bought vs average coin, pooled (t) |
|---|---|---|---|
| 4h | −0.056 / −0.060 / −0.032 / −0.045 | −0.048 (−4.8) | +0.12% (+2.1) |
| 8h | −0.078 / −0.068 / −0.061 / −0.102 | −0.077 (−5.3) | +0.29% (+2.7) |
| 24h | −0.177 / −0.149 / −0.121 / −0.084 | −0.133 (−5.8) | **+0.85% (+2.7)** |

1. IC negative in every window at 8h and 24h: **yes**.
2. Pooled t ≤ −2 at both: **yes** (−5.3, −5.8).
3. 24h excess of the 5 bought > 0.2%: **yes** (+0.85%).

**Rerun with delisted tokens hard-excluded (user request, same day).** Delisted tokens were then purged from every database and are never fetched (7–22 per holdout window). The same test on the purged data still passes all three criteria: IC per window at 24h −0.183 / −0.143 / −0.126 / −0.093; pooled t −5.5 (8h) and −6.0 (24h); the 5 bought beat the average coin by +0.89% per 24h (t 2.8). Note that excluding delisted tokens makes this run slightly survivorship-biased; the original run included them, but its endpoint filtering also requires review; it should not be described as unbiased.

**What the recorded evidence supports.** Out of sample, the coins CalmDip buys beat the **average coin** by about 0.85% per 24h before costs (about 0.65% after an estimated 0.2% round trip). That is a relative edge. A long-only account still carries the whole market's moves, which this test removes; the optional BTC trend filter is the only market-timing element, and it is not covered by this proof. The highest-ranked coins sometimes surge (+8.1% in W−3), so the signal does not support shorting. The sample is 44 non-overlapping days at the 24h hold. The old evaluator dropped coins with missing future prices before ranking, which could replace decision-time picks. The corrected evaluator excludes the entire sample and reports coverage exclusions.

### Walk-forward of the live grid (long-only CalmDip, 32 combinations)

| Data | Verdict | OOS return | Profit factor | Trades | Liquidations |
|---|---|---|---|---|---|
| Latest 14 days | fail (rests on one window) | +13.1% | 1.65 | 41 | 0 |
| Window 1 | **pass** | +1.6% | 1.37 | 21 | 0 |
| Window 2 | fail | −1.8% | 0.76 | 23 | 0 |
| Window 3 | fail (rests on one window) | +1.2% | 1.11 | 22 | 0 |

| Forward test | Return | At 2× costs | Max drawdown | Buy-and-hold, equal-weight universe |
|---|---|---|---|---|
| Window 1 → 2 | −9.8% | −12.5% | 10.0% | +5.9% |
| Window 2 → 3 | +21.9% | +19.3% | 4.8% | +47.1% |

Long only is better than the old market-neutral Pulse (no liquidations, shallower drawdowns), but it trails simply holding the universe, so it is **not a proven edge**. The paper account is the forward test on new data.

## Market-neutral strategy (previous live, for reference)

All numbers come from real Bybit data: closed 15-minute traded and mark-price candles, settled funding, current order books, risk tiers, lot filters and this account's fee rates. They were run on disposable copies of the service's databases, from the account's observed balance (recorded explicitly).

## Tests

```sh
cargo test --release --all-targets   # 69 tests, all pass (2026-10-03)
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
| Pulse (then live) | 0 / 3 | −20.7% | +5.1% |
| Funding | 0 / 3 | −4.2% | −2.5% |
| All together | 0 / 3 | −9.1% | +5.1% |

Switching to the best-looking family on these same windows would have been curve-fitting; the strategy later moved to long-only CalmDip on the strength of the broad-universe signal study and its pre-registered holdout (top of this page), not this table.

## Live service

Long-only CalmDip paper bot on the top 20, restarted with a fresh-data reset after every change. Since 2026-10-03 a background search tests 10,000 combinations (about 20 s), starting 60 minutes after the previous search finishes; paper trades the champion settings that pass the two safety rules, and the walk-forward check is shown as information. The dashboard (http://127.0.0.1:8787) shows the current settings, the search times, positions and P&L; this document records no live performance yet.

## Reproduce

1. Copy `data.db` and `window_1.db` to `window_3.db` from `~/Library/Application Support/BybitMeanReversionBot` with `sqlite3 SRC ".backup DEST"` (never run tools against the live files).
2. Run `validate_cached` on the copy with the observed balance.
3. Run `EQ=<balance> cargo run --release --example research` for the family table.

## Follow-up verification — 2 October 2026

The corrected `signal_ic` was rerun on disposable backups of the four purged holdout databases, with the signal and horizons unchanged. Missing future endpoints now exclude the entire decision-time universe rather than change its membership. No eligible samples were excluded: 148 complete 8h samples and 44 complete 24h samples. Pooled 8h IC is −0.079 (t −5.5), relative excess +0.31%; pooled 24h IC is −0.136 (t −6.0), relative excess +0.89% (t +2.8). Every window's IC remains negative at both horizons.

The registered thresholds still pass on these stored symbols. Excluding delisted tokens leaves survivorship bias, and the reported t-statistics assume independent observations; non-overlapping holds alone do not justify that assumption. These returns omit actual strategy execution and are not account-profit forecasts. The original unpurged databases were not available for this rerun.
