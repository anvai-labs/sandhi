//! Explicit per-call buffered deadline for built-in, in-task transports. This is
//! transport control, never wire metadata. Spawned/custom providers are unsupported.
use std::{future::Future, time::Duration};

tokio::task_local! {
    static BUFFERED_DEADLINE: BufferedDeadline;
}

/// One absolute deadline shared by all buffered transport layers of a call.
/// Scoping does not rebuild clients or reset the deadline at nested boundaries.
#[derive(Clone, Copy, Debug)]
pub struct BufferedDeadline {
    pub(crate) at: tokio::time::Instant,
    pub(crate) timeout: Duration,
}

impl BufferedDeadline {
    pub fn new(timeout: Duration) -> Self {
        Self {
            at: tokio::time::Instant::now() + timeout,
            timeout,
        }
    }

    /// Only built-in transports consume this scope. It is deliberately not
    /// inherited by spawned tasks; callers must reject unsupported extensions.
    pub async fn scope<F: Future>(self, future: F) -> F::Output {
        BUFFERED_DEADLINE.scope(self, future).await
    }

    pub(crate) fn current() -> Option<Self> {
        BUFFERED_DEADLINE.try_with(|deadline| *deadline).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn scoped_deadlines_isolate_concurrent_nested_and_cancelled_calls() {
        let outer = BufferedDeadline::new(Duration::from_secs(5));
        let inner = BufferedDeadline::new(Duration::from_secs(10));
        assert!(BufferedDeadline::current().is_none());
        outer
            .scope(async {
                assert_eq!(BufferedDeadline::current().unwrap().at, outer.at);
                let ((), ()) = tokio::join!(
                    inner.scope(async {
                        tokio::task::yield_now().await;
                        assert_eq!(BufferedDeadline::current().unwrap().at, inner.at);
                    }),
                    async {
                        tokio::task::yield_now().await;
                        assert_eq!(BufferedDeadline::current().unwrap().at, outer.at);
                    }
                );
                let result = tokio::time::timeout(
                    Duration::from_millis(1),
                    inner.scope(std::future::pending::<()>()),
                )
                .await;
                assert!(result.is_err());
                assert_eq!(BufferedDeadline::current().unwrap().at, outer.at);
                assert!(
                    tokio::spawn(async { BufferedDeadline::current().is_none() })
                        .await
                        .unwrap()
                );
            })
            .await;
        assert!(BufferedDeadline::current().is_none());
    }
}
