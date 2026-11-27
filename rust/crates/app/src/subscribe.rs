use crate::live::LiveHandler;
use futures_util::FutureExt;
use grammers_mtsender::{InvocationError, SenderPoolFatHandle, UpdatesConfiguration, UpdatesReceiver};
use grammers_session::updates::UpdatesLike;
use mibot_bot::{BotError, PeerCache};
use mibot_logtail::{Level, Ring};
use mibot_session::MibotSession;
use mibot_tgstate::TgState;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub const SUBSCRIBE_INITIAL_BACKOFF: Duration = Duration::from_secs(2);
pub const SUBSCRIBE_MAX_BACKOFF: Duration = Duration::from_secs(60);

pub type ReadyTrigger = Arc<dyn Fn() + Send + Sync>;

/// Returns true when Telegram rejected the session key permanently (HTTP 401 equivalent).
pub fn is_unauthorized_bot_error(err: &BotError) -> bool {
    err.rpc_error().is_some_and(|rpc| {
        rpc.code == 401
            || matches!(
                rpc.name.as_str(),
                "AUTH_KEY_UNREGISTERED"
                    | "AUTH_KEY_INVALID"
                    | "AUTH_KEY_PERM_EMPTY"
                    | "SESSION_REVOKED"
                    | "SESSION_EXPIRED"
                    | "USER_DEACTIVATED"
                    | "USER_DEACTIVATED_BAN"
            )
    })
}

pub fn is_unauthorized_invocation(err: &InvocationError) -> bool {
    matches!(err, InvocationError::Rpc(rpc) if rpc.code == 401
        || matches!(
            rpc.name.as_str(),
            "AUTH_KEY_UNREGISTERED"
                | "AUTH_KEY_INVALID"
                | "AUTH_KEY_PERM_EMPTY"
                | "SESSION_REVOKED"
                | "SESSION_EXPIRED"
                | "USER_DEACTIVATED"
                | "USER_DEACTIVATED_BAN"
        ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardOutcome {
    Stopped,
    Unauthorized(String),
}

/// Supervises startup readiness (`refresh_self` -> `seed_self` -> `after_ready`), retrying
/// transient network/RPC errors with 2s..60s exponential backoff and halting on 401 auth loss.
pub async fn run_subscribe_guard<F, Fut>(
    cancel: &CancellationToken,
    logs: &Ring,
    after_ready: Option<Arc<dyn Fn() + Send + Sync>>,
    attempt_ready: F,
) -> GuardOutcome
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), (bool, String)>>,
{
    run_subscribe_guard_with_backoff(
        cancel,
        logs,
        after_ready,
        SUBSCRIBE_INITIAL_BACKOFF,
        SUBSCRIBE_MAX_BACKOFF,
        attempt_ready,
    )
    .await
}

pub async fn run_subscribe_guard_with_backoff<F, Fut>(
    cancel: &CancellationToken,
    logs: &Ring,
    after_ready: Option<Arc<dyn Fn() + Send + Sync>>,
    initial_backoff: Duration,
    max_backoff: Duration,
    mut attempt_ready: F,
) -> GuardOutcome
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), (bool, String)>>,
{
    let mut backoff = initial_backoff;
    loop {
        if cancel.is_cancelled() {
            return GuardOutcome::Stopped;
        }
        match attempt_ready().await {
            Ok(()) => {
                if let Some(cb) = &after_ready {
                    cb();
                }
                return GuardOutcome::Stopped;
            }
            Err((true, reason)) => {
                logs.record(Level::Error, "app.unauthorized", &[("err", &reason)]);
                return GuardOutcome::Unauthorized(reason);
            }
            Err((false, reason)) => {
                logs.record(
                    Level::Warn,
                    "app.subscribe_retry",
                    &[
                        ("err", &reason),
                        ("backoff_ms", &backoff.as_millis().to_string()),
                    ],
                );
                tokio::select! {
                    _ = cancel.cancelled() => return GuardOutcome::Stopped,
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(max_backoff);
            }
        }
    }
}

/// Initializes `self_user` + `MibotSession::seed_self` + `TgState::set_self_user_id`, invokes
/// `after_ready`, then drives `UpdatesReceiver::next` safely with state syncing and recovery.
#[allow(clippy::too_many_arguments)]
pub async fn run_updates_supervisor(
    cancel: CancellationToken,
    client: Arc<mibot_bot::Client>,
    session: Arc<MibotSession>,
    state: Option<Arc<TgState>>,
    peers: Arc<PeerCache>,
    logs: Arc<Ring>,
    fat_handle: SenderPoolFatHandle,
    updates_rx: mpsc::Receiver<UpdatesLike>,
    live: Arc<LiveHandler>,
    on_ready: Option<ReadyTrigger>,
) -> GuardOutcome {
    let client_ref = Arc::clone(&client);
    let session_ref = Arc::clone(&session);
    let state_ref = state.clone();
    let ready_res = run_subscribe_guard(&cancel, &logs, None, move || {
        let client = Arc::clone(&client_ref);
        let session = Arc::clone(&session_ref);
        let state = state_ref.clone();
        async move {
            let me = match client.refresh_self().await {
                Ok(u) => u,
                Err(e) => {
                    return Err((is_unauthorized_bot_error(&e), e.to_string()));
                }
            };
            let _ = session.seed_self(me.id, me.access_hash, me.bot);
            if let Some(st) = &state {
                st.set_self_user_id(me.id);
            }
            Ok(())
        }
    })
    .await;

    if let GuardOutcome::Unauthorized(reason) = ready_res {
        cancel.cancel();
        return GuardOutcome::Unauthorized(reason);
    }
    if cancel.is_cancelled() {
        return GuardOutcome::Stopped;
    }

    if let Some(cb) = on_ready {
        let l = Arc::clone(&logs);
        tokio::spawn(async move {
            let res = std::panic::AssertUnwindSafe(async move { cb() })
                .catch_unwind()
                .await;
            if res.is_err() {
                l.record(Level::Error, "app.after_ready_panic", &[]);
            }
        });
    }

    // Channel decoupling `pool.updates` pump from `UpdatesReceiver` so slow catch-up never stalls live dispatch.
    let (fwd_tx, fwd_rx) = mpsc::channel::<UpdatesLike>(1024);
    let pump_live = Arc::clone(&live);
    let pump_handle = tokio::spawn(async move {
        pump_live.run_pump(updates_rx, fwd_tx).await;
    });

    let mut receiver = match UpdatesReceiver::create(
        fat_handle.thin.clone(),
        Arc::clone(&fat_handle.session),
        fwd_rx,
        UpdatesConfiguration { catch_up: true },
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            logs.record(
                Level::Error,
                "updates.receiver_init",
                &[("err", &e.to_string())],
            );
            cancel.cancel();
            let _ = pump_handle.await;
            return GuardOutcome::Stopped;
        }
    };

    let mut sync_tick = tokio::time::interval(Duration::from_secs(5));
    sync_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut backoff = SUBSCRIBE_INITIAL_BACKOFF;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = receiver.sync_update_state().await;
                break;
            }
            _ = sync_tick.tick() => {
                if let Err(e) = receiver.sync_update_state().await {
                    logs.record(Level::Warn, "updates.sync_state", &[("err", &e.to_string())]);
                }
            }
            step = receiver.next() => {
                match step {
                    Ok((items, users, chats)) => {
                        backoff = SUBSCRIBE_INITIAL_BACKOFF;
                        peers.remember_entities(&users, &chats);
                        for (upd, _state) in items {
                            live.dispatch_tl_update(&upd);
                        }
                    }
                    Err(err) => {
                        if is_unauthorized_invocation(&err) {
                            logs.record(Level::Error, "updates.unauthorized", &[("err", &err.to_string())]);
                            let _ = receiver.sync_update_state().await;
                            cancel.cancel();
                            let _ = pump_handle.await;
                            return GuardOutcome::Unauthorized(err.to_string());
                        }
                        if matches!(err, InvocationError::Dropped(_)) {
                            break;
                        }
                        logs.record(
                            Level::Warn,
                            "updates.next_error",
                            &[("err", &err.to_string())],
                        );
                        tokio::select! {
                            _ = cancel.cancelled() => break,
                            _ = tokio::time::sleep(backoff) => {}
                        }
                        backoff = (backoff * 2).min(SUBSCRIBE_MAX_BACKOFF);
                    }
                }
            }
        }
    }

    let _ = receiver.sync_update_state().await;
    drop(receiver);
    let _ = pump_handle.await;
    GuardOutcome::Stopped
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn subscribe_guard_stops_on_unauthorized_without_retrying() {
        let cancel = CancellationToken::new();
        let logs = Arc::new(Ring::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_ref = Arc::clone(&calls);

        let outcome = run_subscribe_guard(&cancel, &logs, None, move || {
            let c = Arc::clone(&calls_ref);
            async move {
                c.fetch_add(1, Ordering::SeqCst);
                Err((true, "401 SESSION_REVOKED".into()))
            }
        })
        .await;

        assert!(matches!(outcome, GuardOutcome::Unauthorized(_)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn subscribe_guard_retries_transient_then_fires_ready_once() {
        let cancel = CancellationToken::new();
        let logs = Arc::new(Ring::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let ready = Arc::new(AtomicUsize::new(0));
        let calls_ref = Arc::clone(&calls);
        let ready_ref = Arc::clone(&ready);

        let outcome = run_subscribe_guard_with_backoff(
            &cancel,
            &logs,
            Some(Arc::new(move || {
                ready_ref.fetch_add(1, Ordering::SeqCst);
            })),
            Duration::from_millis(5),
            Duration::from_millis(20),
            move || {
                let c = Arc::clone(&calls_ref);
                async move {
                    if c.fetch_add(1, Ordering::SeqCst) == 0 {
                        Err((false, "transient".into()))
                    } else {
                        Ok(())
                    }
                }
            },
        )
        .await;
        assert_eq!(outcome, GuardOutcome::Stopped);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(ready.load(Ordering::SeqCst), 1);
    }
}
