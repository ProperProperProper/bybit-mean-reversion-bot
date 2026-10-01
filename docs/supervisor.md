# `src/engine/supervisor.rs`: keeping tasks alive

Status (2 October 2026): aborting a task does not stop a `spawn_blocking` walk-forward already running, but that work ends at its 15-minute deadline, inside the 20-minute stall limit.

Every background task (the bar loop, the web console) runs under supervision. If a task **returns an error, panics, or stops sending heartbeats** for longer than its `stall_after`, it is logged, aborted and restarted.

- **Backoff:** waits start at 5 s and double up to 5 min.
- **Reset:** the wait drops back to 5 s after a run that lasted over 10 minutes.

| Item | What it does |
|---|---|
| `struct TaskHealth` | Public snapshot of a task: restarts, last error, seconds since its last heartbeat, running. Shown on the console. |
| `struct Entry` (private) | The internal record behind each task's health |
| `struct Health` | Shared map of task name → entry |
| `struct Heartbeat` / `beat()` | Given to each task; calling `beat()` records "still alive" |
| `snapshot()` | Every task's `TaskHealth` |
| `with(name, f)` (private) | Runs `f` on a task's entry, creating it if needed |
| `spawn(name, stall_after, make_task)` | Starts `make_task(heartbeat)` in a loop. A watchdog checks every 5 s and aborts the task if its last heartbeat is older than `stall_after`. Whatever ended the run (exit, error, panic or hang) is recorded, followed by the backoff and a restart. |

Tests check that a failing task is restarted and that a hung task is detected, aborted and restarted. On top of this, the service has a process-level watchdog that exits if the bar loop is silent for 40 minutes, so launchd restarts the whole process (see [bot.md](bot.md)).
