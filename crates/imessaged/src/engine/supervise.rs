//! Supervision of long-lived background tasks (receive loop, state monitors).
//!
//! rustpush's message-handling path has many `unwrap`/`expect`/`panic!`
//! sites. A panic inside a plain `tokio::spawn` silently kills that task
//! while everything else keeps reporting "connected". [`supervise`] runs a
//! task in its own `tokio::spawn`, awaits its `JoinHandle`, and on a panic or
//! an `Err` reports the failure and respawns it with exponential backoff
//! ([`Backoff::standard`]: 1 s doubling to a 30 s cap). It stops on a normal
//! `Ok(())` exit, when the factory returns `None` (owner gone / superseded),
//! or when the shutdown signal fires; dropping or aborting the supervisor
//! aborts the child as well ([`AbortOnDrop`]).

use std::any::Any;
use std::future::Future;
use std::time::{Duration, Instant};

use tokio::sync::watch;
use tokio::task::JoinHandle;

/// A task that ran at least this long before failing resets the backoff.
pub const HEALTHY_RUN: Duration = Duration::from_secs(60);

/// Exponential backoff: `initial`, doubling, capped at `cap`.
#[derive(Debug, Clone)]
pub struct Backoff {
    initial: Duration,
    cap: Duration,
    current: Duration,
}

impl Backoff {
    pub const fn new(initial: Duration, cap: Duration) -> Backoff {
        Backoff { initial, cap, current: initial }
    }

    /// 1 s -> 2 -> 4 -> 8 -> 16 -> 30 -> 30 ...
    pub const fn standard() -> Backoff {
        Backoff::new(Duration::from_secs(1), Duration::from_secs(30))
    }

    /// The delay to wait now; advances the schedule.
    pub fn next_delay(&mut self) -> Duration {
        let d = self.current;
        self.current = (self.current * 2).min(self.cap);
        d
    }

    pub fn reset(&mut self) {
        self.current = self.initial;
    }
}

/// Aborts the wrapped task when dropped.
#[derive(Debug)]
pub struct AbortOnDrop<T>(pub JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn panic_message(p: Box<dyn Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".into()
    }
}

/// Run `make()`'s future under supervision until it exits cleanly, `make`
/// returns `None`, or `shutdown` becomes `true` (or its sender is dropped).
///
/// `on_failure(reason, delay)` is called after each panic / `Err` before
/// the backoff sleep; use it to log, emit an error event and mark the
/// connection degraded. `make` is called again for the restart (so it can
/// re-subscribe to channels and clear the degraded mark).
pub async fn supervise<F, Fut, E>(name: &'static str, mut make: F, mut on_failure: E, mut shutdown: watch::Receiver<bool>, mut backoff: Backoff)
where
    F: FnMut() -> Option<Fut> + Send,
    Fut: Future<Output = Result<(), String>> + Send + 'static,
    E: FnMut(&str, Duration) + Send,
{
    loop {
        if *shutdown.borrow() {
            return;
        }
        let Some(fut) = make() else { return };
        let started = Instant::now();
        let mut task = AbortOnDrop(tokio::spawn(fut));
        let result = tokio::select! {
            r = &mut task.0 => r,
            _ = shutdown.wait_for(|v| *v) => return,
        };
        let reason = match result {
            Ok(Ok(())) => return,
            Ok(Err(e)) => format!("{name} failed: {e}"),
            Err(e) if e.is_panic() => format!("{name} panicked: {}", panic_message(e.into_panic())),
            Err(_) => return, // cancelled from outside
        };
        if started.elapsed() >= HEALTHY_RUN {
            backoff.reset();
        }
        let delay = backoff.next_delay();
        on_failure(&reason, delay);
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            _ = shutdown.wait_for(|v| *v) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    #[test]
    fn backoff_schedule() {
        let mut b = Backoff::standard();
        let got: Vec<u64> = (0..8).map(|_| b.next_delay().as_secs()).collect();
        assert_eq!(got, vec![1, 2, 4, 8, 16, 30, 30, 30]);
        b.reset();
        assert_eq!(b.next_delay(), Duration::from_secs(1));
    }

    #[tokio::test]
    async fn restarts_after_panic_and_error_then_stops_on_clean_exit() {
        let runs = Arc::new(AtomicUsize::new(0));
        let failures = Arc::new(Mutex::new(Vec::<(String, Duration)>::new()));
        let (_tx, rx) = watch::channel(false);
        let r = runs.clone();
        let f = failures.clone();
        supervise(
            "loop",
            move || {
                let n = r.fetch_add(1, Ordering::SeqCst);
                Some(async move {
                    match n {
                        0 => panic!("boom {n}"),
                        1 => Err("bad".to_string()),
                        _ => Ok(()),
                    }
                })
            },
            move |reason, delay| f.lock().unwrap().push((reason.to_string(), delay)),
            rx,
            Backoff::new(Duration::from_millis(5), Duration::from_millis(8)),
        )
        .await;
        assert_eq!(runs.load(Ordering::SeqCst), 3);
        let f = failures.lock().unwrap();
        assert_eq!(f.len(), 2);
        assert!(f[0].0.contains("panicked") && f[0].0.contains("boom 0"), "{:?}", f[0]);
        assert_eq!(f[0].1, Duration::from_millis(5));
        assert!(f[1].0.contains("failed: bad"));
        assert_eq!(f[1].1, Duration::from_millis(8));
    }

    #[tokio::test]
    async fn shutdown_stops_supervisor_and_aborts_child() {
        struct Flag(Arc<AtomicUsize>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = watch::channel(false);
        let d = dropped.clone();
        let sup = tokio::spawn(supervise(
            "forever",
            move || {
                let flag = Flag(d.clone());
                Some(async move {
                    let _flag = flag;
                    std::future::pending::<()>().await;
                    Ok(())
                })
            },
            |_, _| panic!("no failure expected"),
            rx,
            Backoff::standard(),
        ));
        tokio::time::sleep(Duration::from_millis(20)).await;
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), sup).await.unwrap().unwrap();
        // the child is aborted (its future dropped) once the supervisor returns
        tokio::time::timeout(Duration::from_secs(5), async {
            while dropped.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        // `None` from the factory (owner gone / superseded) stops immediately.
        let (_tx, rx) = watch::channel(false);
        supervise("gone", || None::<std::future::Ready<Result<(), String>>>, |_, _| {}, rx, Backoff::standard()).await;
    }
}
