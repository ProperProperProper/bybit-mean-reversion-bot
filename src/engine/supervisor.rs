//! Keeps every background task running. A task that returns an error, panics,
//! or stops sending heartbeats for longer than its `stall_after` is logged,
//! aborted and restarted with exponential backoff (5s doubling to 5 min).

use log::{error, info, warn};
use serde::Serialize;
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::Instant;

#[derive(Debug, Clone, Serialize)]
pub struct TaskHealth {
    pub restarts: u32,
    pub last_error: Option<String>,
    pub seconds_since_heartbeat: u64,
    pub running: bool,
}

struct Entry {
    restarts: u32,
    last_error: Option<String>,
    last_beat: Instant,
    running: bool,
}

#[derive(Clone, Default)]
pub struct Health(Arc<Mutex<BTreeMap<String, Entry>>>);

#[derive(Clone)]
pub struct Heartbeat {
    health: Health,
    name: String,
}

impl Heartbeat {
    pub fn beat(&self) {
        if let Some(e) = self
            .health
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_mut(&self.name)
        {
            e.last_beat = Instant::now();
        }
    }
}

impl Health {
    pub fn snapshot(&self) -> BTreeMap<String, TaskHealth> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|(k, e)| {
                (
                    k.clone(),
                    TaskHealth {
                        restarts: e.restarts,
                        last_error: e.last_error.clone(),
                        seconds_since_heartbeat: e.last_beat.elapsed().as_secs(),
                        running: e.running,
                    },
                )
            })
            .collect()
    }

    fn with<R>(&self, name: &str, f: impl FnOnce(&mut Entry) -> R) -> R {
        let mut map = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let e = map.entry(name.to_string()).or_insert(Entry {
            restarts: 0,
            last_error: None,
            last_beat: Instant::now(),
            running: false,
        });
        f(e)
    }

    // NOTE(agents): A task cannot beat while it awaits a spawn_blocking job, so its stall limit
    //               must exceed that job's deadline: search_task uses SEARCH_DEADLINE + 10 min
    //               (service.rs), or a long search gets killed mid-run. bar_task no longer runs
    //               the search.
    /// Spawn `make_task` under supervision. The task must call `hb.beat()` at
    /// least every `stall_after`, or it is treated as hung and restarted.
    pub fn spawn<F, Fut>(&self, name: &str, stall_after: Duration, make_task: F)
    where
        F: Fn(Heartbeat) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = anyhow::Result<()>> + Send + 'static,
    {
        let health = self.clone();
        let name = name.to_string();
        tokio::spawn(async move {
            let mut backoff = Duration::from_secs(5);
            loop {
                health.with(&name, |e| {
                    e.running = true;
                    e.last_beat = Instant::now();
                });
                let hb = Heartbeat {
                    health: health.clone(),
                    name: name.clone(),
                };
                let handle = tokio::spawn(make_task(hb));
                let abort = handle.abort_handle();
                let started = Instant::now();

                let watchdog_health = health.clone();
                let watchdog_name = name.clone();
                let watchdog = tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        let stale = watchdog_health.with(&watchdog_name, |e| e.last_beat.elapsed());
                        if stale > stall_after {
                            warn!(
                                "[supervisor] {watchdog_name} silent for {}s — aborting as hung",
                                stale.as_secs()
                            );
                            abort.abort();
                            return;
                        }
                    }
                });

                let outcome = handle.await;
                watchdog.abort();
                let err = match outcome {
                    Ok(Ok(())) => "exited".to_string(),
                    Ok(Err(e)) => format!("error: {e:#}"),
                    Err(e) if e.is_cancelled() => "hung (no heartbeat), aborted".to_string(),
                    Err(e) => format!("panic: {e}"),
                };
                error!(
                    "[supervisor] {name} stopped ({err}); restarting in {}s",
                    backoff.as_secs()
                );
                health.with(&name, |e| {
                    e.running = false;
                    e.restarts += 1;
                    e.last_error = Some(err);
                });
                tokio::time::sleep(backoff).await;
                backoff = if started.elapsed() > Duration::from_secs(600) {
                    Duration::from_secs(5)
                } else {
                    (backoff * 2).min(Duration::from_secs(300))
                };
                info!("[supervisor] restarting {name}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test(start_paused = true)]
    async fn failing_task_is_restarted() {
        let health = Health::default();
        let runs = Arc::new(AtomicU32::new(0));
        let r = runs.clone();
        health.spawn("flaky", Duration::from_secs(60), move |_hb| {
            let r = r.clone();
            async move {
                r.fetch_add(1, Ordering::SeqCst);
                anyhow::bail!("boom")
            }
        });
        tokio::time::sleep(Duration::from_secs(20)).await;
        assert!(runs.load(Ordering::SeqCst) >= 2);
        assert!(health.snapshot()["flaky"].restarts >= 1);
    }

    #[tokio::test(start_paused = true)]
    async fn hung_task_is_aborted_and_restarted() {
        let health = Health::default();
        let runs = Arc::new(AtomicU32::new(0));
        let r = runs.clone();
        health.spawn("stuck", Duration::from_secs(10), move |_hb| {
            let r = r.clone();
            async move {
                r.fetch_add(1, Ordering::SeqCst);
                std::future::pending::<()>().await;
                Ok(())
            }
        });
        tokio::time::sleep(Duration::from_secs(40)).await;
        assert!(runs.load(Ordering::SeqCst) >= 2);
        assert!(health.snapshot()["stuck"]
            .last_error
            .as_deref()
            .unwrap()
            .contains("hung"));
    }
}
