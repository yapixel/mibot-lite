use futures_util::FutureExt;
use mibot_bot::BoxFuture;
use std::collections::{HashMap, VecDeque};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

pub const COMMAND_WORKERS: usize = 16;
pub const COMMAND_QUEUE: usize = 64;
pub const TASK_WORKERS: usize = 4;
pub const TASK_QUEUE: usize = 16;
pub const DELAYED_WORKERS: usize = 4;
pub const DELAYED_QUEUE: usize = 256;
pub const LONG_TASKS: usize = 4;
pub const MAX_TIMERS: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuiesceTimeout;

struct ReleaseGuard {
    release: Option<Box<dyn FnOnce() + Send>>,
}

impl ReleaseGuard {
    fn new(release: Option<Box<dyn FnOnce() + Send>>) -> Self {
        Self { release }
    }
}

impl Drop for ReleaseGuard {
    fn drop(&mut self) {
        if let Some(cb) = self.release.take() {
            cb();
        }
    }
}

type JobFn = Box<dyn FnOnce(CancellationToken) -> BoxFuture<'static, ()> + Send>;

struct Job {
    cancel: CancellationToken,
    run: JobFn,
    _guard: ReleaseGuard,
}

struct PoolInner {
    queue: VecDeque<Job>,
    workers: usize,
    active_jobs: usize,
}

#[derive(Clone)]
struct Pool {
    name: &'static str,
    max_workers: usize,
    queue_cap: usize,
    state: Arc<Mutex<PoolInner>>,
    idle_notify: Arc<Notify>,
    on_panic: Arc<Mutex<Option<PanicHook>>>,
}

pub type PanicHook = Arc<dyn Fn(&str, &str) + Send + Sync>;

impl Pool {
    fn new(
        name: &'static str,
        max_workers: usize,
        queue_cap: usize,
        idle_notify: Arc<Notify>,
        on_panic: Arc<Mutex<Option<PanicHook>>>,
    ) -> Self {
        Self {
            name,
            max_workers,
            queue_cap,
            state: Arc::new(Mutex::new(PoolInner {
                queue: VecDeque::new(),
                workers: 0,
                active_jobs: 0,
            })),
            idle_notify,
            on_panic,
        }
    }

    fn busy(&self) -> usize {
        let g = self.state.lock().unwrap_or_else(|e| e.into_inner());
        g.active_jobs + g.queue.len()
    }

    fn admit(
        &self,
        cancel: CancellationToken,
        release: Option<Box<dyn FnOnce() + Send>>,
        task: impl FnOnce(CancellationToken) -> BoxFuture<'static, ()> + Send + 'static,
    ) -> bool {
        let guard = ReleaseGuard::new(release);
        let mut g = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if self.queue_cap == 0 {
            if g.active_jobs + g.queue.len() >= self.max_workers {
                drop(g);
                drop(guard);
                return false;
            }
        } else if g.queue.len() >= self.queue_cap {
            drop(g);
            drop(guard);
            return false;
        }
        g.queue.push_back(Job {
            cancel,
            run: Box::new(task),
            _guard: guard,
        });
        if g.workers < self.max_workers {
            g.workers += 1;
            let pool = self.clone();
            tokio::spawn(async move {
                pool.worker_loop().await;
            });
        }
        true
    }

    async fn worker_loop(self) {
        loop {
            let job = {
                let mut g = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(job) = g.queue.pop_front() {
                    g.active_jobs += 1;
                    job
                } else {
                    g.workers = g.workers.saturating_sub(1);
                    if g.active_jobs == 0 && g.queue.is_empty() {
                        self.idle_notify.notify_waiters();
                    }
                    return;
                }
            };

            let Job {
                cancel,
                run,
                _guard,
            } = job;
            if !cancel.is_cancelled() {
                let outcome = AssertUnwindSafe(run(cancel)).catch_unwind().await;
                if let Err(payload) = outcome {
                    let msg = panic_payload_message(payload);
                    if let Some(hook) = self
                        .on_panic
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone()
                    {
                        hook(self.name, &msg);
                    }
                }
            }
            drop(_guard);

            let mut g = self.state.lock().unwrap_or_else(|e| e.into_inner());
            g.active_jobs = g.active_jobs.saturating_sub(1);
            if g.active_jobs == 0 && g.queue.is_empty() {
                self.idle_notify.notify_waiters();
            }
        }
    }
}

fn panic_payload_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic payload".into()
    }
}

struct GateInner {
    closed: bool,
    active_tokens: HashMap<u64, CancellationToken>,
    next_id: u64,
}

#[derive(Clone)]
pub struct Scheduler {
    commands: Pool,
    tasks: Pool,
    delayed: Pool,
    long: Pool,
    gate: Arc<Mutex<GateInner>>,
    timers: Arc<AtomicUsize>,
    tracked: Arc<AtomicUsize>,
    next_timer_id: Arc<AtomicU64>,
    idle_notify: Arc<Notify>,
    on_panic: Arc<Mutex<Option<PanicHook>>>,
}

/// RAII guard for external background jobs counted in `Scheduler::busy()` until dropped.
pub struct WorkGuard {
    tracked: Arc<AtomicUsize>,
    idle_notify: Arc<Notify>,
}

impl Drop for WorkGuard {
    fn drop(&mut self) {
        self.tracked.fetch_sub(1, Ordering::AcqRel);
        self.idle_notify.notify_waiters();
    }
}

impl Default for Scheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl Scheduler {
    pub fn new() -> Self {
        let idle_notify = Arc::new(Notify::new());
        let on_panic: Arc<Mutex<Option<PanicHook>>> = Arc::new(Mutex::new(None));
        Self {
            commands: Pool::new(
                "command",
                COMMAND_WORKERS,
                COMMAND_QUEUE,
                Arc::clone(&idle_notify),
                Arc::clone(&on_panic),
            ),
            tasks: Pool::new(
                "background",
                TASK_WORKERS,
                TASK_QUEUE,
                Arc::clone(&idle_notify),
                Arc::clone(&on_panic),
            ),
            delayed: Pool::new(
                "delayed",
                DELAYED_WORKERS,
                DELAYED_QUEUE,
                Arc::clone(&idle_notify),
                Arc::clone(&on_panic),
            ),
            long: Pool::new(
                "long",
                LONG_TASKS,
                0,
                Arc::clone(&idle_notify),
                Arc::clone(&on_panic),
            ),
            gate: Arc::new(Mutex::new(GateInner {
                closed: false,
                active_tokens: HashMap::new(),
                next_id: 1,
            })),
            timers: Arc::new(AtomicUsize::new(0)),
            tracked: Arc::new(AtomicUsize::new(0)),
            next_timer_id: Arc::new(AtomicU64::new(1)),
            idle_notify,
            on_panic,
        }
    }

    pub fn set_panic_hook(&self, hook: PanicHook) {
        *self.on_panic.lock().unwrap_or_else(|e| e.into_inner()) = Some(hook);
    }

    fn register_token(&self, parent: &CancellationToken) -> Option<(u64, CancellationToken)> {
        let mut g = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        if g.closed {
            return None;
        }
        let id = g.next_id;
        g.next_id += 1;
        let child = parent.child_token();
        g.active_tokens.insert(id, child.clone());
        Some((id, child))
    }

    fn unregister_token(&self, id: u64) {
        let mut g = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        g.active_tokens.remove(&id);
    }

    /// Submits a command handler with an optional `release` callback guaranteed to run once
    /// even if the job is rejected/cancelled/panics before completion.
    pub fn submit(
        &self,
        cancel: CancellationToken,
        release: Option<Box<dyn FnOnce() + Send>>,
        task: impl FnOnce(CancellationToken) -> BoxFuture<'static, ()> + Send + 'static,
    ) -> bool {
        let Some((id, child)) = self.register_token(&cancel) else {
            if let Some(cb) = release {
                cb();
            }
            return false;
        };
        let gate = Arc::clone(&self.gate);
        let wrapped_release: Box<dyn FnOnce() + Send> = Box::new(move || {
            gate.lock()
                .unwrap_or_else(|e| e.into_inner())
                .active_tokens
                .remove(&id);
            if let Some(cb) = release {
                cb();
            }
        });
        self.commands.admit(child, Some(wrapped_release), task)
    }

    pub fn background(
        &self,
        cancel: CancellationToken,
        task: impl FnOnce(CancellationToken) -> BoxFuture<'static, ()> + Send + 'static,
    ) -> bool {
        let Some((id, child)) = self.register_token(&cancel) else {
            return false;
        };
        let gate = Arc::clone(&self.gate);
        let release: Box<dyn FnOnce() + Send> = Box::new(move || {
            gate.lock()
                .unwrap_or_else(|e| e.into_inner())
                .active_tokens
                .remove(&id);
        });
        self.tasks.admit(child, Some(release), task)
    }

    pub fn long_running(
        &self,
        cancel: CancellationToken,
        task: impl FnOnce(CancellationToken) -> BoxFuture<'static, ()> + Send + 'static,
    ) -> bool {
        let Some((id, child)) = self.register_token(&cancel) else {
            return false;
        };
        let gate = Arc::clone(&self.gate);
        let release: Box<dyn FnOnce() + Send> = Box::new(move || {
            gate.lock()
                .unwrap_or_else(|e| e.into_inner())
                .active_tokens
                .remove(&id);
        });
        self.long.admit(child, Some(release), task)
    }

    pub fn after(
        &self,
        cancel: CancellationToken,
        delay: Duration,
        task: impl FnOnce(CancellationToken) -> BoxFuture<'static, ()> + Send + 'static,
    ) -> bool {
        let prev = self.timers.load(Ordering::Acquire);
        if prev >= MAX_TIMERS {
            return false;
        }
        let Some((id, child)) = self.register_token(&cancel) else {
            return false;
        };
        if self.timers.fetch_add(1, Ordering::AcqRel) >= MAX_TIMERS {
            self.timers.fetch_sub(1, Ordering::AcqRel);
            self.unregister_token(id);
            return false;
        }
        let _timer_seq = self.next_timer_id.fetch_add(1, Ordering::Relaxed);
        let sched = self.clone();
        tokio::spawn(async move {
            struct TimerDrop {
                sched: Scheduler,
                id: u64,
                disarmed: bool,
            }
            impl Drop for TimerDrop {
                fn drop(&mut self) {
                    self.sched.timers.fetch_sub(1, Ordering::AcqRel);
                    if !self.disarmed {
                        self.sched.unregister_token(self.id);
                    }
                }
            }
            let mut td = TimerDrop {
                sched: sched.clone(),
                id,
                disarmed: false,
            };
            tokio::select! {
                _ = child.cancelled() => {
                    return;
                }
                _ = tokio::time::sleep(delay) => {}
            }
            let gate = Arc::clone(&sched.gate);
            let release: Box<dyn FnOnce() + Send> = Box::new(move || {
                gate.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .active_tokens
                    .remove(&id);
            });
            // Keep token managed by job's release guard once admitted to delayed pool.
            td.disarmed = true;
            drop(td);
            let _ = sched.delayed.admit(child, Some(release), task);
        });
        true
    }

    /// Aborts all currently tracked queued/running jobs and pending delayed timers.
    pub fn abort(&self) -> usize {
        let tokens: Vec<CancellationToken> = {
            let g = self.gate.lock().unwrap_or_else(|e| e.into_inner());
            g.active_tokens.values().cloned().collect()
        };
        let count = tokens.len();
        for tok in tokens {
            tok.cancel();
        }
        count
    }

    pub fn track_work(&self) -> Option<WorkGuard> {
        let g = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        if g.closed {
            return None;
        }
        self.tracked.fetch_add(1, Ordering::AcqRel);
        Some(WorkGuard {
            tracked: Arc::clone(&self.tracked),
            idle_notify: Arc::clone(&self.idle_notify),
        })
    }

    pub fn busy(&self) -> usize {
        self.commands.busy()
            + self.tasks.busy()
            + self.delayed.busy()
            + self.long.busy()
            + self.tracked.load(Ordering::Acquire)
    }

    pub fn long_busy(&self) -> usize {
        self.long.busy()
    }

    /// Closes admission gate and waits up to `timeout` for active/queued workers to drain.
    /// Reopens the gate if `timeout` elapses before reaching zero.
    pub async fn quiesce(&self, timeout: Duration) -> Result<(), QuiesceTimeout> {
        {
            let mut g = self.gate.lock().unwrap_or_else(|e| e.into_inner());
            g.closed = true;
        }
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.busy() == 0 {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                self.resume();
                return Err(QuiesceTimeout);
            }
            let notified = self.idle_notify.notified();
            let step = remaining.min(Duration::from_millis(20));
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(step) => {}
            }
        }
    }

    pub fn resume(&self) {
        let mut g = self.gate.lock().unwrap_or_else(|e| e.into_inner());
        g.closed = false;
    }

    pub fn is_closed(&self) -> bool {
        self.gate.lock().unwrap_or_else(|e| e.into_inner()).closed
    }

    pub async fn wait_idle(&self) {
        while self.busy() > 0 || self.timers.load(Ordering::Acquire) > 0 {
            let notified = self.idle_notify.notified();
            tokio::select! {
                _ = notified => {}
                _ = tokio::time::sleep(Duration::from_millis(10)) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::Barrier;

    #[tokio::test]
    async fn bounds_overflow_long_isolation_and_release_guarantee() {
        let sched = Scheduler::new();
        let release_count = Arc::new(AtomicUsize::new(0));
        let blocker = Arc::new(Barrier::new(COMMAND_WORKERS + 1));
        let started = Arc::new(AtomicUsize::new(0));

        // Occupy all 16 command workers and wait until every worker has pulled its job.
        for _ in 0..COMMAND_WORKERS {
            let b = Arc::clone(&blocker);
            let s = Arc::clone(&started);
            let rel = Arc::clone(&release_count);
            assert!(sched.submit(
                CancellationToken::new(),
                Some(Box::new(move || {
                    rel.fetch_add(1, Ordering::SeqCst);
                })),
                move |_| {
                    Box::pin(async move {
                        s.fetch_add(1, Ordering::SeqCst);
                        b.wait().await;
                    })
                },
            ));
        }
        while started.load(Ordering::SeqCst) < COMMAND_WORKERS {
            tokio::task::yield_now().await;
        }

        // Fill all 64 queued command slots.
        for _ in 0..COMMAND_QUEUE {
            let rel = Arc::clone(&release_count);
            assert!(sched.submit(
                CancellationToken::new(),
                Some(Box::new(move || {
                    rel.fetch_add(1, Ordering::SeqCst);
                })),
                |_| Box::pin(async {}),
            ));
        }

        // 81st command is rejected immediately, and its release still fires.
        let rel = Arc::clone(&release_count);
        assert!(!sched.submit(
            CancellationToken::new(),
            Some(Box::new(move || {
                rel.fetch_add(1, Ordering::SeqCst);
            })),
            |_| Box::pin(async {}),
        ));
        assert_eq!(release_count.load(Ordering::SeqCst), 1);

        // Background task pool remains independent while commands are blocked.
        let bg_done = Arc::new(AtomicUsize::new(0));
        let bg_clone = Arc::clone(&bg_done);
        assert!(sched.background(CancellationToken::new(), move |_| {
            Box::pin(async move {
                bg_clone.fetch_add(1, Ordering::SeqCst);
            })
        }));

        // Let commands run through.
        blocker.wait().await;
        sched.wait_idle().await;
        assert_eq!(
            release_count.load(Ordering::SeqCst),
            COMMAND_WORKERS + COMMAND_QUEUE + 1
        );
        assert_eq!(bg_done.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn long_pool_isolation_and_panic_catch_and_quiesce_abort() {
        let sched = Scheduler::new();
        let panic_seen = Arc::new(Mutex::new(false));
        let p_clone = Arc::clone(&panic_seen);
        sched.set_panic_hook(Arc::new(move |_, _| {
            *p_clone.lock().unwrap() = true;
        }));

        let long_blocker = Arc::new(Barrier::new(LONG_TASKS + 1));
        for _ in 0..LONG_TASKS {
            let b = Arc::clone(&long_blocker);
            assert!(sched.long_running(CancellationToken::new(), move |_| {
                Box::pin(async move {
                    b.wait().await;
                })
            }));
        }
        // 5th long task is rejected.
        assert!(!sched.long_running(CancellationToken::new(), |_| Box::pin(async {})));

        // Background task still executes even while all LONG_TASKS are held!
        let bg_done = Arc::new(AtomicUsize::new(0));
        let bg_clone = Arc::clone(&bg_done);
        assert!(sched.background(CancellationToken::new(), move |_| {
            Box::pin(async move {
                bg_clone.fetch_add(1, Ordering::SeqCst);
            })
        }));
        while bg_done.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // Release long tasks.
        long_blocker.wait().await;
        sched.wait_idle().await;

        // Panicking command doesn't wedge worker pool or miss release callback.
        let rel = Arc::new(AtomicUsize::new(0));
        let rel_clone = Arc::clone(&rel);
        assert!(sched.submit(
            CancellationToken::new(),
            Some(Box::new(move || {
                rel_clone.fetch_add(1, Ordering::SeqCst);
            })),
            |_| Box::pin(async move {
                panic!("boom");
            }),
        ));
        sched.wait_idle().await;
        assert!(*panic_seen.lock().unwrap());
        assert_eq!(rel.load(Ordering::SeqCst), 1);

        // Test abort + quiesce timeout -> resume -> clean quiesce.
        let hold_cancel = Arc::new(Notify::new());
        let hc = Arc::clone(&hold_cancel);
        assert!(sched.submit(
            CancellationToken::new(),
            None,
            move |tok| {
                Box::pin(async move {
                    hc.notify_one();
                    tok.cancelled().await;
                })
            },
        ));
        hold_cancel.notified().await;
        assert!(sched.quiesce(Duration::from_millis(30)).await.is_err());
        assert!(!sched.is_closed()); // Reopened after timeout.

        assert!(sched.abort() >= 1);
        assert!(sched.quiesce(Duration::from_secs(1)).await.is_ok());
        assert!(sched.is_closed());
        assert!(!sched.submit(CancellationToken::new(), None, |_| Box::pin(async {})));
        sched.resume();
        assert!(!sched.is_closed());
    }
}
