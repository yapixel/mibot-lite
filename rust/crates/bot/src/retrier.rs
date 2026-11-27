use crate::{BoxFuture, RpcTransport};
use grammers_mtsender::{InvocationError, RpcError};
use grammers_tl_types::{self as tl, Identifiable};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// Clock + jitter abstraction allowing deterministic `Retrier` unit tests.
pub trait Clock: Send + Sync {
    fn now(&self) -> Instant;
    fn sleep(&self, dur: Duration) -> BoxFuture<'static, ()>;
    fn jitter(&self, base: Duration) -> Duration;
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, dur: Duration) -> BoxFuture<'static, ()> {
        Box::pin(tokio::time::sleep(dur))
    }

    fn jitter(&self, base: Duration) -> Duration {
        let cap = (base.as_nanos() / 4).min(u128::from(u64::MAX)) as u64;
        if cap == 0 {
            return Duration::ZERO;
        }
        let mut buf = [0u8; 8];
        let _ = getrandom::getrandom(&mut buf);
        let n = u64::from_le_bytes(buf) % (cap + 1);
        Duration::from_nanos(n)
    }
}

#[derive(Clone)]
struct Cooldown {
    until: Instant,
    err: RpcError,
}

/// Async MTProto RPC retry handler sharing per-method `FLOOD_WAIT` cooldowns
/// without holding its mutex across `.await` sleeps.
pub struct Retrier {
    pub max_wait: Duration,
    pub attempts: usize,
    clock: Arc<dyn Clock>,
    blocked: Mutex<HashMap<u32, Cooldown>>,
    now_unix: Arc<dyn Fn() -> i64 + Send + Sync>,
}

impl Default for Retrier {
    fn default() -> Self {
        Self::new(Duration::from_secs(60), 5)
    }
}

impl Retrier {
    pub fn new(max_wait: Duration, attempts: usize) -> Self {
        Self::with_clock(
            max_wait,
            attempts,
            Arc::new(SystemClock),
            Arc::new(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0)
            }),
        )
    }

    pub fn with_clock(
        max_wait: Duration,
        attempts: usize,
        clock: Arc<dyn Clock>,
        now_unix: Arc<dyn Fn() -> i64 + Send + Sync>,
    ) -> Self {
        Self {
            max_wait,
            attempts,
            clock,
            blocked: Mutex::new(HashMap::new()),
            now_unix,
        }
    }

    /// Returns true if `constructor_id` is self-recovered by `UpdatesReceiver`
    /// (`updates.getChannelDifference`) and must not be retried here.
    pub fn is_self_recovering(constructor_id: u32) -> bool {
        constructor_id == tl::functions::updates::GetChannelDifference::CONSTRUCTOR_ID
    }

    pub fn is_server_failure(err: &InvocationError) -> bool {
        match err {
            InvocationError::Rpc(rpc) => {
                rpc.code >= 500 || rpc.is("RPC_CALL_FAIL") || rpc.is("RPC_MCGET_FAIL")
            }
            _ => false,
        }
    }

    pub fn flood_wait(err: &InvocationError) -> Option<Duration> {
        match err {
            InvocationError::Rpc(rpc) if rpc.code == 420 => {
                rpc.value.map(|secs| Duration::from_secs(u64::from(secs)))
            }
            _ => None,
        }
    }

    pub fn method_name(constructor_id: u32) -> &'static str {
        tl::name_for_id(constructor_id)
    }

    pub async fn invoke(
        &self,
        transport: &dyn RpcTransport,
        dc_id: Option<i32>,
        constructor_id: u32,
        body: Vec<u8>,
        edit_banned_until: Option<i32>,
    ) -> Result<Vec<u8>, InvocationError> {
        if Self::is_self_recovering(constructor_id) {
            return transport.invoke_raw(dc_id, constructor_id, body).await;
        }

        let mut server_errors = 0usize;
        for attempt in 0usize.. {
            // Re-check shared cooldown after sleeping; another concurrent caller may extend it.
            // Never hold `blocked` mutex across `.await`.
            loop {
                let wait_entry = {
                    let mut map = self.blocked.lock().await;
                    let now = self.clock.now();
                    if let Some(entry) = map.get(&constructor_id).cloned() {
                        if entry.until > now {
                            Some((entry.until.duration_since(now), entry.err))
                        } else {
                            map.remove(&constructor_id);
                            None
                        }
                    } else {
                        None
                    }
                };
                match wait_entry {
                    None => break,
                    Some((wait, err)) => {
                        if wait > self.max_wait + Duration::from_secs(1) {
                            return Err(InvocationError::Rpc(err));
                        }
                        self.clock.sleep(wait).await;
                    }
                }
            }

            // Telegram treats remaining ban durations < 30s as permanent.
            if let Some(until) = edit_banned_until
                && until != 0
                && i64::from(until) - (self.now_unix)() < 30
            {
                return Err(InvocationError::Rpc(RpcError {
                    code: 400,
                    name: mibot_i18n::tr(
                        "Remaining mute/ban duration is under 30s; aborted, please set a longer duration",
                        "临时限制剩余不足 30 秒，已停止发送，请重新设置时长",
                    )
                    .into(),
                    value: None,
                    caused_by: Some(constructor_id),
                }));
            }

            match transport
                .invoke_raw(dc_id, constructor_id, body.clone())
                .await
            {
                Ok(resp) => return Ok(resp),
                Err(err) => {
                    if let Some(flood) = Self::flood_wait(&err) {
                        let wait = flood + Duration::from_secs(1);
                        if let InvocationError::Rpc(rpc) = &err {
                            let mut map = self.blocked.lock().await;
                            let at = self.clock.now();
                            map.retain(|_, e| e.until > at);
                            let until = at + wait;
                            let update = map
                                .get(&constructor_id)
                                .is_none_or(|cur| until > cur.until);
                            if update {
                                map.insert(
                                    constructor_id,
                                    Cooldown {
                                        until,
                                        err: rpc.clone(),
                                    },
                                );
                            }
                        }
                        if attempt >= self.attempts || flood > self.max_wait {
                            return Err(err);
                        }
                        continue;
                    }
                    if !Self::is_server_failure(&err) || attempt >= self.attempts {
                        return Err(err);
                    }
                    let shift = server_errors.min(3) as u32;
                    let delay = Duration::from_secs(2u64 << shift);
                    server_errors += 1;
                    let wait = delay + self.clock.jitter(delay);
                    self.clock.sleep(wait).await;
                }
            }
        }
        unreachable!("loop only exits via return")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Mutex as AsyncMutex;

    struct FakeClock {
        now: std::sync::Mutex<Instant>,
        slept: std::sync::Mutex<Duration>,
    }

    impl FakeClock {
        fn new() -> Self {
            Self {
                now: std::sync::Mutex::new(Instant::now()),
                slept: std::sync::Mutex::new(Duration::ZERO),
            }
        }

        fn slept(&self) -> Duration {
            *self.slept.lock().unwrap()
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> Instant {
            *self.now.lock().unwrap()
        }

        fn sleep(&self, dur: Duration) -> BoxFuture<'static, ()> {
            *self.now.lock().unwrap() += dur;
            *self.slept.lock().unwrap() += dur;
            Box::pin(async {})
        }

        fn jitter(&self, _base: Duration) -> Duration {
            Duration::ZERO
        }
    }

    enum FlakyKind {
        Flood(u32),
        Server,
        Other(&'static str),
    }

    struct FlakyTransport {
        failures: usize,
        kind: FlakyKind,
        calls: AtomicUsize,
        seen_bodies: AsyncMutex<Vec<Vec<u8>>>,
    }

    impl FlakyTransport {
        fn new(failures: usize, kind: FlakyKind) -> Self {
            Self {
                failures,
                kind,
                calls: AtomicUsize::new(0),
                seen_bodies: AsyncMutex::new(Vec::new()),
            }
        }
    }

    impl RpcTransport for FlakyTransport {
        fn invoke_raw<'a>(
            &'a self,
            _dc_id: Option<i32>,
            constructor_id: u32,
            body: Vec<u8>,
        ) -> BoxFuture<'a, Result<Vec<u8>, InvocationError>> {
            Box::pin(async move {
                self.seen_bodies.lock().await.push(body);
                let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
                if n <= self.failures {
                    return Err(match self.kind {
                        FlakyKind::Flood(secs) => InvocationError::Rpc(RpcError {
                            code: 420,
                            name: "FLOOD_WAIT".into(),
                            value: Some(secs),
                            caused_by: Some(constructor_id),
                        }),
                        FlakyKind::Server => InvocationError::Rpc(RpcError {
                            code: 500,
                            name: "RPC_CALL_FAIL".into(),
                            value: None,
                            caused_by: Some(constructor_id),
                        }),
                        FlakyKind::Other(name) => InvocationError::Rpc(RpcError {
                            code: 400,
                            name: name.into(),
                            value: None,
                            caused_by: Some(constructor_id),
                        }),
                    });
                }
                Ok(Vec::new())
            })
        }
    }

    #[tokio::test]
    async fn retrier_flood_and_server_and_self_recovering() {
        let cases = [
            (1, FlakyKind::Flood(3), 2, false, 4),
            (3, FlakyKind::Flood(2), 4, false, 9),
            (5, FlakyKind::Flood(2), 4, true, 9),
            (1, FlakyKind::Flood(60), 1, true, 0),
            (0, FlakyKind::Flood(0), 1, false, 0),
            (2, FlakyKind::Server, 3, false, 6),
        ];
        for (failures, kind, want_calls, want_err, want_slept_secs) in cases {
            let clock = Arc::new(FakeClock::new());
            let r = Retrier::with_clock(
                Duration::from_secs(30),
                3,
                clock.clone(),
                Arc::new(|| 1_800_000_000),
            );
            let t = FlakyTransport::new(failures, kind);
            let res = r
                .invoke(
                    &t,
                    None,
                    tl::functions::messages::GetMessages::CONSTRUCTOR_ID,
                    vec![1, 2, 3],
                    None,
                )
                .await;
            assert_eq!(res.is_err(), want_err);
            assert_eq!(t.calls.load(Ordering::SeqCst), want_calls);
            assert_eq!(clock.slept(), Duration::from_secs(want_slept_secs));
        }

        // Ordinary 400 RPC errors are not retried.
        let clock = Arc::new(FakeClock::new());
        let r = Retrier::with_clock(
            Duration::from_secs(30),
            3,
            clock.clone(),
            Arc::new(|| 1_800_000_000),
        );
        let t = FlakyTransport::new(1, FlakyKind::Other("USER_NOT_PARTICIPANT"));
        assert!(
            r.invoke(
                &t,
                None,
                tl::functions::messages::GetMessages::CONSTRUCTOR_ID,
                vec![],
                None
            )
            .await
            .is_err()
        );
        assert_eq!(t.calls.load(Ordering::SeqCst), 1);

        // updates.getChannelDifference is self-recovering and bypasses retries.
        for kind in [FlakyKind::Flood(3), FlakyKind::Server] {
            let clock = Arc::new(FakeClock::new());
            let r = Retrier::with_clock(
                Duration::from_secs(30),
                3,
                clock.clone(),
                Arc::new(|| 1_800_000_000),
            );
            let t = FlakyTransport::new(1, kind);
            assert!(
                r.invoke(
                    &t,
                    None,
                    tl::functions::updates::GetChannelDifference::CONSTRUCTOR_ID,
                    vec![],
                    None
                )
                .await
                .is_err()
            );
            assert_eq!(t.calls.load(Ordering::SeqCst), 1);
            assert_eq!(clock.slept(), Duration::ZERO);
        }

        assert_eq!(
            Retrier::method_name(tl::functions::channels::GetMessages::CONSTRUCTOR_ID),
            "channels.getMessages"
        );
    }

    #[tokio::test]
    async fn retrier_shares_cooldown_and_edit_banned_guard_and_preserves_body() {
        let clock = Arc::new(FakeClock::new());
        let unix = Arc::new(std::sync::atomic::AtomicI64::new(1_800_000_000));
        let unix_ref = unix.clone();
        let r = Retrier::with_clock(
            Duration::from_secs(60),
            0,
            clock.clone(),
            Arc::new(move || unix_ref.load(Ordering::SeqCst)),
        );
        let t = FlakyTransport::new(1, FlakyKind::Flood(2));
        // First call gets FLOOD_WAIT_2 and fails because attempts=0.
        assert!(
            r.invoke(
                &t,
                None,
                tl::functions::messages::GetMessages::CONSTRUCTOR_ID,
                vec![9],
                None
            )
            .await
            .is_err()
        );
        // Unrelated method is not blocked.
        assert!(
            r.invoke(
                &t,
                None,
                tl::functions::contacts::ResolveUsername::CONSTRUCTOR_ID,
                vec![8],
                None
            )
            .await
            .is_ok()
        );
        assert_eq!(clock.slept(), Duration::ZERO);
        // Same method waits out the 2 + 1 = 3s shared cooldown before calling transport.
        assert!(
            r.invoke(
                &t,
                None,
                tl::functions::messages::GetMessages::CONSTRUCTOR_ID,
                vec![9],
                None
            )
            .await
            .is_ok()
        );
        assert_eq!(clock.slept(), Duration::from_secs(3));

        // EditBanned remaining < 30s check.
        let r2 = Retrier::with_clock(
            Duration::from_secs(60),
            1,
            clock.clone(),
            Arc::new(|| 1_800_000_040),
        );
        let t2 = FlakyTransport::new(0, FlakyKind::Server);
        let res = r2
            .invoke(
                &t2,
                None,
                tl::functions::channels::EditBanned::CONSTRUCTOR_ID,
                vec![],
                Some(1_800_000_060),
            )
            .await;
        assert!(res.is_err());
        assert_eq!(t2.calls.load(Ordering::SeqCst), 0);

        // Retries preserve exact serialized bytes (including random_id).
        let r3 = Retrier::with_clock(
            Duration::from_secs(60),
            1,
            clock,
            Arc::new(|| 1_800_000_000),
        );
        let t3 = FlakyTransport::new(1, FlakyKind::Server);
        r3.invoke(
            &t3,
            None,
            tl::functions::messages::SendMessage::CONSTRUCTOR_ID,
            vec![42, 99, 7],
            None,
        )
        .await
        .unwrap();
        let bodies = t3.seen_bodies.lock().await;
        assert_eq!(*bodies, vec![vec![42, 99, 7], vec![42, 99, 7]]);
    }
}
