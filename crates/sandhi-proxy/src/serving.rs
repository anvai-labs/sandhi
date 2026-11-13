//! Public HTTP serving owns its bounded durable recovery and final inventory.
//! No source inference or process termination belongs to this supervisor.
use crate::{settlement::buffered, ConnectionPolicy, ProxyState, SchemeListeners};
use std::{io, sync::Arc, time::Duration};

#[derive(Debug)]
struct AccountingIncomplete;
impl std::fmt::Display for AccountingIncomplete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "accounting shutdown incomplete; durable uncertainty or partial inventory remains",
        )
    }
}
impl std::error::Error for AccountingIncomplete {}

/// Whether serving returned an incomplete shutdown rather than clean settlement.
/// Includes an expired drain deadline and retained durable accounting uncertainty;
/// the standalone binary preserves exit status 124 for either case.
#[must_use]
pub fn is_incomplete_shutdown(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::TimedOut
        || error
            .get_ref()
            .is_some_and(|cause| cause.is::<AccountingIncomplete>())
}

/// Dropping a join handle alone detaches it. Always request cancellation, even if
/// the supervisor panics or the embedder shuts down its runtime during cleanup.
struct RecoveryTask(Option<tokio::task::JoinHandle<()>>);
impl Drop for RecoveryTask {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}
impl RecoveryTask {
    fn start(state: Arc<ProxyState>) -> Self {
        Self(Some(tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    biased;
                    _ = state.lifecycle.cancelled() => break,
                    _ = tick.tick() => {
                        if !state.lifecycle.is_running() {
                            break;
                        }
                        if let Err(reason) = buffered::recover(state.clone()).await {
                            tracing::warn!(reason, "durable accounting recovery incomplete");
                        }
                    }
                }
            }
        })))
    }

    async fn stop(&mut self, deadline: std::time::Instant) -> io::Result<()> {
        if let Some(task) = self.0.take() {
            task.abort();
            // Cancelling the waiter does not cancel already dispatched blocking
            // ledger work. Its operation guard remains part of wait_idle below.
            let _ = tokio::time::timeout_at(deadline.into(), task)
                .await
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "recovery cancellation deadline exceeded",
                    )
                })?;
        }
        Ok(())
    }
}

/// A panicking host shutdown future or cancelled runtime must still close
/// dispatch authorization; async cleanup cannot be promised after runtime teardown.
struct ServingCutoff {
    lifecycle: Arc<crate::lifecycle::Lifecycle>,
    grace: Duration,
}
impl Drop for ServingCutoff {
    fn drop(&mut self) {
        self.lifecycle.begin_quiesce(self.grace);
    }
}

pub(crate) async fn supervise<F>(
    state: Arc<ProxyState>,
    listeners: SchemeListeners,
    shutdown: F,
    grace: Duration,
) -> io::Result<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    if !state.lifecycle.claim_serving() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "server lifecycle already used",
        ));
    }
    // All sockets have bound before this point. A bind failure starts neither a
    // driver nor a supervisor and leaves the state available for a corrected bind.
    let (cancel_on_drop, cancelled) = tokio::sync::oneshot::channel::<()>();
    let cutoff = ServingCutoff {
        lifecycle: state.lifecycle.clone(),
        grace,
    };
    let task = tokio::spawn(async move {
        let _cutoff = cutoff;
        let shutdown = async move {
            tokio::select! {
                _ = shutdown => {},
                _ = cancelled => {},
            }
        };
        let mut recovery = RecoveryTask::start(state.clone());
        let policy = ConnectionPolicy::from_state(&state);
        let served = crate::serve_router_listener_with_shutdown(
            crate::build_app(state.clone()),
            listeners,
            shutdown,
            grace,
            policy,
        )
        .await;
        let deadline = state.lifecycle.begin_quiesce(grace);
        state.lifecycle.seal_completion();
        let cleaned = async {
            recovery.stop(deadline).await?;
            tokio::time::timeout_at(deadline.into(), state.lifecycle.wait_idle())
                .await
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "shutdown deadline reached with admitted work still cleaning up",
                    )
                })?;
            match buffered::finish_shutdown(state.clone()).await {
                Ok(true) => Ok(()),
                Ok(false) => {
                    tracing::error!(
                        retained = buffered::retained(&state),
                        "durable accounting shutdown incomplete"
                    );
                    Err(io::Error::other(AccountingIncomplete))
                }
                Err(reason) => {
                    tracing::error!(reason, "durable accounting shutdown check failed");
                    if std::time::Instant::now() >= deadline {
                        Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "accounting shutdown deadline exceeded",
                        ))
                    } else {
                        Err(io::Error::other(AccountingIncomplete))
                    }
                }
            }
        }
        .await;
        let result = served.and(cleaned);
        if result.is_ok() && std::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "shutdown deadline exhausted during cleanup",
            ));
        }
        if result.is_ok() {
            state.lifecycle.stop();
        }
        result
    });
    // The receiver interprets sender drop as shutdown. Retain this guard across
    // await so cancellation of the public future cannot detach a running server.
    let _cancel_on_drop = cancel_on_drop;
    task.await.map_err(|_| {
        tracing::error!("serving supervisor interrupted; cleanup may remain");
        io::Error::other(AccountingIncomplete)
    })?
}
