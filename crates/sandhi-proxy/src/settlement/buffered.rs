//! Opt-in buffered HTTP ownership and bounded recovery.
use crate::ProxyState;
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Copy)]
pub struct Config {
    capacity: usize,
    accounting_wait: Duration,
    transport_timeout: Duration,
}
impl Config {
    pub fn new(
        capacity: usize,
        accounting_wait: Duration,
        transport_timeout: Duration,
    ) -> Result<Self, &'static str> {
        if capacity == 0
            || capacity > 1024
            || accounting_wait.is_zero()
            || accounting_wait > Duration::from_secs(30)
            || transport_timeout.is_zero()
            || transport_timeout > Duration::from_secs(600)
        {
            return Err("invalid buffered accounting bounds");
        }
        Ok(Self {
            capacity,
            accounting_wait,
            transport_timeout,
        })
    }
}
pub fn enable(state: &Arc<ProxyState>, config: Config) -> Result<(), &'static str> {
    let config = Config::new(
        config.capacity,
        config.accounting_wait,
        config.transport_timeout,
    )?;
    super::with_durable_ledger(&state.ledger, |ledger| ledger.validate_tracked_durable())
        .map_err(|_| "tracked accounting requires an available single file-backed ledger")?;
    state
        .buffered_accounting
        .set(Arc::new(Buffered {
            jobs: Jobs::for_proxy(state, config.capacity),
            config,
            recovery: Mutex::new(Recovery::default()),
        }))
        .map_err(|_| "tracked accounting already enabled")
}

use super::{
    admission::{AdmissionOutcome, PreparedAdmission},
    jobs::{Jobs, Obligation, Outcome, Work},
    AuthorizedExecution, DispatchAttempt,
};
use crate::*;
use sandhi_store::ledger::evidence::{RecoveryCursor, RecoveryState, ScopeCursor};
use std::sync::Mutex;

pub(crate) struct Accounting {
    pub obligation: Obligation,
    pub execution: AuthorizedExecution,
    pub wait: Duration,
}
#[derive(Default)]
struct Recovery {
    scopes: Option<ScopeCursor>,
    current: Option<String>,
    page: Option<RecoveryCursor>,
    retained_after: Option<u64>,
    complete: bool,
    unresolved: usize,
}
pub(crate) struct Buffered {
    config: Config,
    jobs: Jobs,
    recovery: Mutex<Recovery>,
}

pub fn retained(state: &ProxyState) -> usize {
    state
        .buffered_accounting
        .get()
        .map_or(0, |owner| owner.jobs.retained())
}

pub(crate) fn accounting_error(dialect: IngressDialect, request_id: &str) -> Response {
    let mut response = ingress_error(
        dialect,
        StatusCode::BAD_GATEWAY,
        "accounting unresolved; upstream may have completed; do not automatically retry",
    );
    response.headers_mut().insert(
        "x-sandhi-request-id",
        request_id.parse().expect("generated identity"),
    );
    response
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn handle(
    state: Arc<ProxyState>,
    owner: Arc<Buffered>,
    provider: ProviderHandle,
    request: ChatRequestV1,
    body: Bytes,
    dialect: IngressDialect,
    permit: Arc<AdmissionPermit>,
    wants_stream: bool,
    transparent_eligible: bool,
    scope: String,
    policy: Policy,
    ceiling: u64,
    _effective_max: u64,
    input_len: usize,
    configured_deadline: Option<Duration>,
    streaming_limits: Option<crate::deadlines::StreamLimits>,
) -> Response {
    if (wants_stream
        && (streaming_limits.is_none()
            || !transparent_eligible
            || !matches!(dialect, IngressDialect::OpenAi)))
        || !matches!(dialect, IngressDialect::OpenAi | IngressDialect::Anthropic)
        || !provider.supports_owned_buffered()
        || policy != Policy::Block
        || request.metadata.idempotency_key.is_some()
        || request.max_output_tokens.is_none()
    {
        return ingress_error(dialect, StatusCode::BAD_REQUEST,
            "tracked accounting requires buffered chat or transparent OpenAI Chat streaming with a configured streaming deadline, a retry-free built-in OpenAI-compatible provider, Block policy, explicit output limit and no idempotency key");
    }
    let transport = if wants_stream {
        streaming_limits
            .expect("validated streaming policy")
            .dispatch_duration()
    } else {
        configured_deadline.unwrap_or(owner.config.transport_timeout)
    };
    if !wants_stream && transport > Duration::from_secs(600) {
        return ingress_error(
            dialect,
            StatusCode::BAD_REQUEST,
            "tracked transport deadline exceeds 600 seconds",
        );
    }
    let transparent = transparent_eligible;
    let mut accounting = RequestAccounting::new(
        state.clone(),
        scope.clone(),
        None,
        provider.slug().into(),
        &request,
        dialect_label(dialect),
        if transparent {
            metrics::Plane::Transparent
        } else {
            metrics::Plane::Translation
        },
    );
    accounting.input_len = input_len;
    if wants_stream {
        accounting.stream_body_lifetime = Some(
            streaming_limits
                .expect("validated streaming policy")
                .body_lifetime(),
        );
    }
    accounting.finalized = true; // no event for refused or unknown admission
    let request_id = accounting.request_id.clone();
    let mut obligation = match owner.jobs.obligation() {
        Ok(owned) => owned,
        Err(_) => {
            return ingress_error(
                dialect,
                StatusCode::SERVICE_UNAVAILABLE,
                "tracked accounting capacity unavailable",
            )
        }
    };
    let wait = owner.config.accounting_wait;
    let task_id = request_id.clone();
    // The owner and admission permit move before the first await; disconnects do
    // not cancel an admission, provider call, terminal capture or settlement.
    let task = tokio::spawn(async move {
        let prepared = PreparedAdmission::new(
            task_id.clone(),
            scope,
            ceiling,
            time::Duration::seconds((transport + wait * 3).as_secs() as i64 + 60),
            100_000,
        )
        .expect("validated generated admission")
        .correlated();
        let Ok(ticket) = obligation.admit(prepared) else {
            return accounting_error(dialect, &task_id);
        };
        if tokio::time::timeout(wait, ticket.wait()).await.is_err() {
            return accounting_error(dialect, &task_id);
        }
        let pending = match ticket.take() {
            Some(AdmissionOutcome::Prepared(pending)) => pending,
            Some(AdmissionOutcome::Denied { .. }) => {
                return ingress_error(dialect, StatusCode::TOO_MANY_REQUESTS, "budget exhausted")
            }
            _ => return accounting_error(dialect, &task_id),
        };
        let ticket = match obligation.submit(Work::Authorize(pending)) {
            Ok(ticket) => ticket,
            Err(rejected) => {
                // Only a scheduling refusal before authorization can prove no send.
                if let Work::Authorize(pending) = rejected.work {
                    if let Ok(ticket) = obligation.submit(Work::Close(pending)) {
                        if tokio::time::timeout(wait, ticket.wait()).await.is_ok() {
                            let _ = ticket.take();
                        }
                    }
                }
                return accounting_error(dialect, &task_id);
            }
        };
        if tokio::time::timeout(wait, ticket.wait()).await.is_err() {
            return accounting_error(dialect, &task_id);
        }
        let Some(Outcome::Dispatch(DispatchAttempt::Authorized(execution))) = ticket.take() else {
            return accounting_error(dialect, &task_id);
        };
        // Authorizing SQLite work can cross cutoff. Do not dispatch a late permit
        // or pretend the durable MayHaveDispatched fence can be rolled back.
        let Some(operation) = state.lifecycle.try_operation() else {
            return accounting_error(dialect, &task_id);
        };
        accounting.reservation = Some(execution.intent().reservation.clone());
        accounting.operation = Some(operation);
        accounting.owned = Some(Accounting {
            obligation,
            execution,
            wait,
        });
        accounting.finalized = false;
        let full_error_detail = state.error_detail_full;
        let dispatch = async move {
            if wants_stream {
                transparent_stream_response(
                    provider,
                    body,
                    request.metadata.session_id.clone(),
                    dialect,
                    accounting,
                    full_error_detail,
                    None,
                    permit,
                )
                .await
            } else if transparent {
                transparent_complete_response(
                    provider,
                    body,
                    request.metadata.session_id.clone(),
                    dialect,
                    accounting,
                    full_error_detail,
                    None,
                    permit,
                )
                .await
            } else {
                complete_response(
                    provider,
                    request,
                    dialect,
                    accounting,
                    full_error_detail,
                    permit,
                )
                .await
            }
        };
        // Tokio task-local transport policies do not propagate through spawn.
        if wants_stream {
            streaming_limits
                .expect("validated streaming policy")
                .transport()
                .scope(dispatch)
                .await
        } else {
            sandhi_providers::BufferedDeadline::new(transport)
                .scope(dispatch)
                .await
        }
    });
    let mut response = match tokio::time::timeout(transport + wait * 3, task).await {
        Ok(Ok(response)) => response,
        _ => accounting_error(dialect, &request_id),
    };
    response.headers_mut().insert(
        "x-sandhi-request-id",
        request_id.parse().expect("generated identity"),
    );
    response
}

/// One bounded recovery slice. Only canonical ready records can settle; no inference
/// or usage-event replay. Dropping this waiter does not cancel the blocking work.
pub async fn recover(state: Arc<ProxyState>) -> Result<usize, &'static str> {
    recover_inner(state, false).await
}

/// Final bounded inventory after all admitted operations drain. A partial sweep,
/// unresolved durable row or retained owner is incomplete, even with zero workers.
/// The original shutdown deadline is never extended; SQLite work may outlive this
/// waiter, so standalone uses its process watchdog for the actual termination bound.
pub async fn finish_shutdown(state: Arc<ProxyState>) -> Result<bool, &'static str> {
    if state.buffered_accounting.get().is_none() {
        return Ok(true);
    }
    if state.lifecycle.is_running() || state.lifecycle.active_operations() != 0 {
        return Err("accounting has not drained");
    }
    let deadline = state.lifecycle.deadline().ok_or("shutdown not started")?;
    tokio::time::timeout_at(deadline.into(), recover_inner(state.clone(), true))
        .await
        .map_err(|_| "accounting shutdown deadline exceeded")??;
    let owner = state.buffered_accounting.get().expect("enabled");
    let cursor = owner
        .recovery
        .try_lock()
        .map_err(|_| "recovery still active")?;
    Ok(cursor.complete && cursor.unresolved == 0 && owner.jobs.retained() == 0)
}

async fn recover_inner(state: Arc<ProxyState>, shutdown: bool) -> Result<usize, &'static str> {
    let Some(owner) = state.buffered_accounting.get().cloned() else {
        return Ok(0);
    };
    let operation = if shutdown {
        None
    } else {
        Some(state.lifecycle.try_operation().ok_or("accounting closed")?)
    };
    tokio::task::spawn_blocking(move || {
        let _operation = operation;
        let mut cursor = owner
            .recovery
            .try_lock()
            .map_err(|_| "recovery already running")?;
        if shutdown {
            *cursor = Recovery::default();
        }
        if cursor.current.is_none() && cursor.scopes.is_none() {
            cursor.unresolved = 0;
        }
        cursor.complete = false;
        let started = std::time::Instant::now();
        let mut settled = 0;
        for _ in 0..8 {
            if started.elapsed() >= Duration::from_secs(1)
                || (!shutdown && !state.lifecycle.is_running())
                || state
                    .lifecycle
                    .remaining()
                    .is_some_and(|left| left.is_zero())
            {
                break;
            }
            owner.jobs.retry_terminal(&mut cursor.retained_after);
            let result = super::with_durable_ledger(&state.ledger, |ledger| {
                if cursor.current.is_none() {
                    let inventory = ledger.recovery_scopes_durable(cursor.scopes.as_ref(), 1)?;
                    cursor.scopes = inventory.next;
                    cursor.current = inventory.scopes.into_iter().next();
                }
                let Some(scope) = cursor.current.clone() else {
                    return Ok(0);
                };
                let page = ledger.recovery_page_durable(&scope, cursor.page.as_ref(), 32)?;
                let mut count = 0;
                for entry in page.entries {
                    if matches!(entry.state, RecoveryState::ReadyToSettle) {
                        ledger.settle_terminal_durable(&scope, &entry.execution_id)?;
                        count += 1;
                    } else if !matches!(
                        entry.state,
                        RecoveryState::Settled(_) | RecoveryState::ClosedBeforeDispatch(_)
                    ) {
                        cursor.unresolved += 1;
                    }
                }
                cursor.page = page.next;
                if cursor.page.is_none() {
                    cursor.current = None;
                }
                Ok(count)
            })
            .map_err(|_| "accounting recovery unresolved")?;
            settled += result;
            if cursor.current.is_none() && cursor.scopes.is_none() {
                cursor.complete = true;
                break;
            }
        }
        Ok(settled)
    })
    .await
    .map_err(|_| "accounting worker interrupted")?
}
