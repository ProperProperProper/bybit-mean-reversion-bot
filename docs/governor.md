# `src/engine/governor.rs`: CPU cap and deadlines

Status (2 October 2026): the governor targets a CPU level at cooperative checkpoints; it is not a hard cap.

Targets **85% CPU** at cooperative checkpoints, where 100% equals one full core, rather than enforcing a hard process-wide limit. It checks deadlines between units of heavy work.

## Constants

| Name | Value | Meaning |
|---|---|---|
| `CPU_TARGET_PCT` | 85 | Target CPU use of the whole process |
| `SAMPLE_EVERY` | 500 ms | How often CPU use is measured |
| `WORK_SLICE` | 20 ms | Minimum work between pauses |
| `MAX_PAUSE_FRACTION` | 0.95 | The controller never pauses more than 95% of the time |

## Items

| Item | What it does |
|---|---|
| `struct Governor` | Holds the measured CPU % and the current pause fraction (atomics, shared by all threads) |
| `struct DeadlineExceeded` (+ `Display::fmt`, `Error`) | The error `checkpoint` returns once a deadline has passed; `fmt` prints "deadline exceeded" |
| `load(a)` / `store(a, v)` (private) | Read and write an `f64` kept in an `AtomicU64` |
| `process_cpu_seconds()` | User plus system CPU seconds used by this process (`getrusage`) |
| `LAST_PAUSE` (thread-local) | When this thread last paused |
| `cpu_pct()` / `pause_fraction()` | The current measurement, and the controller output |
| `update(cpu)` (private) | Integral controller: `pause += 0.004 × (cpu − 85)`, clamped to 0–0.95 |
| `checkpoint(deadline)` | Called in heavy loops (the walk-forward). Errors if the deadline has passed. Otherwise, once at least 20 ms of work has run, it sleeps `work × p / (1 − p)` (at most 500 ms), so CPU settles near the target. |
| `global()` | The single process-wide governor. The first call starts the `cpu-governor` thread, which measures CPU every 500 ms and calls `update`. |

**Why the trading loop never calls `checkpoint`:** only heavy research-style work, like the walk-forward, is slowed, but paper processing waits for that selection to finish. Tests check that a busy loop is held near the target and that an expired deadline aborts.
