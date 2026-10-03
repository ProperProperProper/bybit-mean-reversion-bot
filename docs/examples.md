# `examples/`: research tools

Status (2 October 2026): results from these tools on the corrected engine are in [validation.md](validation.md).

The original three tools are read-only towards Bybit. They need the Keychain credentials ([keychain.md](keychain.md)) for fee rates and the real balance.

## `fetch_research_data.rs`: `main()`

Builds the research data:

1. Prints the real account balance.
2. Lists today's top 20 token USDT perpetuals by 24h turnover (never delisted ones).
3. Measures their Bybit rules now (fails rather than storing an incomplete snapshot) and keeps those with complete rules, up to 20, as the universe, storing their launch times.
4. For each of the three 14-day windows ([research.md](research.md)), fetches any missing traded and mark candles and the settled funding into `window_k.db` (6 coins at a time, paced), recording a new listing's first trade.

Any symbol still missing a candle after its listing start fails the run.

**`HOLDOUT=k,k,…` mode:** for windows `k` (e.g. −3…0, before window 1), fetches traded candles for every USDT perpetual token trading today with no delisting scheduled into `holdout/window_k.db`, for never-seen tests with `signal_ic`. No credentials.

**Delisted tokens are hard-excluded** in both modes: they are never fetched, and `Cache::retain_symbols` deletes any stored rows of tokens that are not trading (delisted, delisting, or non-token contracts).

## `research.rs`: `main()`

Signal research, from the real balance (or `EQ=…`).

1. Loads the three windows and joins them for forward tests.
2. Runs each of the eight signal families, and all of them together, through a **full-window search in every 14-day window** (no split). It prints the best usable settings' 14-day net, profit factor and trades; these are in-sample by construction.
3. Runs **two 14-day forward tests**: settings chosen on window k trade window k+1, which they never saw, at real and at 2x costs.

Long-only families by default (`NEUTRAL=1` for the market-neutral ones), plus buy-and-hold references (BTC and the equal-weight universe) for each traded window. Set `EQ=` to the observed balance to run without network access. The windows hold today's top coins, so long-only results there are biased upwards; use `signal_ic` on the full universe for entry evidence.

## `signal_ic.rs`: entry-signal study

`cargo run --release --example signal_ic [-- WINDOW_DIR]`. For every signal and holding period (4h, 8h, 24h, non-overlapping), it reports the mean rank correlation (IC) between the signal and each coin's next-open-to-close return relative to the average coin, its t-stat, and the excess return of the 5 lowest and 5 highest values. Pass a directory of windows holding candles for every trading token (e.g. `holdout/` created by `HOLDOUT=` mode); the universe is then the top 20 by turnover at each bar rather than today's top 20. `WINDOWS=` picks windows, `SIGNAL=` one signal. Missing future endpoints exclude an entire frozen decision-time sample, and the output reports the exclusion count. Results remain conditional on coverage and stored-symbol selection. The t-statistic assumes independent observations. No network.

## `validate_cached.rs`: offline current-grid validation

`cargo run --release --example validate_cached -- SNAPSHOT_DIR OBSERVED_EQUITY output.json`

Loads `data.db` plus `window_1.db` through `window_3.db` from a disposable snapshot. The equity argument must be finite and positive and should come from an observed account value. It runs the live full-window search (`walkforward::LIVE_COMBOS` combinations, about 20 minutes each) on the latest cache and the three research windows, then two chronological forward tests (chosen on window k, traded on window k+1) at normal and doubled fee/book costs.

The JSON records data coverage, parameters, reports, returns, costs, drawdown and liquidations. `coverage` counts leading/internal/trailing absent candles without claiming their cause. `outcome` aggregates simulated metrics/fees/funding. The tool makes no API, Keychain or order call. It opens the copies in SQLite WAL mode; it should not run against the live service directory.
