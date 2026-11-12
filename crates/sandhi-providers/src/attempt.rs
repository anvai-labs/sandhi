//! Opt-in physical-attempt observations (TD-0026 W05b).
//!
//! This module records facts at the provider transport boundary. It deliberately does not
//! persist, export, price, reserve, or settle anything: those policies belong to W05c-e. An
//! observation contains bounded correlation and neutral usage only; request bodies, headers,
//! credentials, and caller attribution never enter this contract.

use std::fmt;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::{SystemTime, UNIX_EPOCH};

use futures_core::Stream;
use serde::{Deserialize, Serialize};

use crate::{ByteStream, ParsedUsage, ProviderError, ProviderResponse, StreamChunk};
use sandhi_core::UsageCompleteness;

/// Version of the internal observation shape. This is not an external wire-schema version.
pub const ATTEMPT_OBSERVATION_VERSION: u8 = 1;

const MAX_EXECUTION_ID_BYTES: usize = 192;
const MAX_PROVIDER_BYTES: usize = 96;
const MAX_MODEL_BYTES: usize = 256;
const MAX_PROVIDER_REQUEST_ID_BYTES: usize = 256;
const MAX_CHANNEL_CAPACITY: usize = 65_536;

/// Terminal transport outcome. Measurement completeness is recorded independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptOutcome {
    Success,
    ProviderRejected,
    TransportError,
    Timeout,
    Cancelled,
    /// The stream ended without the adapter's synthetic terminal measurement item.
    IncompleteStream,
}

/// A lifecycle observation for one actual adapter send.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptObservation {
    pub version: u8,
    pub execution_id: String,
    pub attempt_id: String,
    pub attempt_ordinal: u64,
    pub provider: String,
    pub model: Option<String>,
    pub observed_at_unix_ms: i64,
    pub phase: AttemptPhase,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttemptPhase {
    /// The adapter invoked its HTTP transport. This proves a physical gateway attempt, not that
    /// the provider received or billed the request.
    Dispatch,
    Terminal {
        outcome: AttemptOutcome,
        provider_status: Option<u16>,
        provider_request_id: Option<String>,
        usage: Option<ParsedUsage>,
        usage_completeness: UsageCompleteness,
    },
}

/// Receiving half of Sandhi's bounded, non-blocking attempt channel.
///
/// Consumers drain this off the request task. W05b intentionally supplies no persistence worker;
/// W05c-e own durable failure policy, replay, and export.
pub struct AttemptReceiver {
    receiver: Receiver<AttemptObservation>,
}

impl fmt::Debug for AttemptReceiver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AttemptReceiver").finish_non_exhaustive()
    }
}

impl AttemptReceiver {
    pub fn try_recv(&self) -> Result<AttemptObservation, TryRecvError> {
        self.receiver.try_recv()
    }

    #[must_use]
    pub fn drain(&self) -> Vec<AttemptObservation> {
        self.receiver.try_iter().collect()
    }
}

struct SharedAttemptContext {
    execution_id: String,
    nonce: [u8; 16],
    sender: SyncSender<AttemptObservation>,
    next_ordinal: AtomicU64,
    dropped: AtomicU64,
    single_attempt_id: Option<String>,
    qualified_response: OnceLock<QualifiedResponseEvidence>,
}

/// Strict counts from one completed JSON response, independent of subsequent
/// codec/projection success. Numeric evidence only; no provider body is retained.
#[derive(Debug, Clone)]
pub struct QualifiedBufferedEvidence {
    pub endpoint: sandhi_core::EndpointFamilyV1,
    pub upstream_request_id: Option<String>,
    pub usage: Result<sandhi_core::UsageV2, sandhi_core::usage::BufferedUsageError>,
}

enum QualifiedResponseEvidence {
    Buffered(Box<QualifiedBufferedEvidence>),
    Stream(crate::raw::QualifiedStreamObservation),
}

struct AttemptCancellation {
    reason: AtomicU8,
    parent: Option<Arc<AttemptCancellation>>,
}

/// Per-execution context carried outside provider bodies and headers.
#[derive(Clone)]
pub struct AttemptContext {
    shared: Arc<SharedAttemptContext>,
    cancellation: Arc<AttemptCancellation>,
}

impl fmt::Debug for AttemptContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AttemptContext")
            .field("execution_id", &self.shared.execution_id)
            .finish_non_exhaustive()
    }
}

impl AttemptContext {
    /// Construct a context from a caller-minted opaque execution id.
    ///
    /// The id is metadata, never authority. Empty, control-bearing, or oversized values are
    /// rejected so downstream observation cannot become a log-injection or memory channel.
    pub fn channel(
        execution_id: impl Into<String>,
        capacity: usize,
    ) -> Result<(Self, AttemptReceiver), AttemptContextError> {
        let execution_id = execution_id.into();
        validate_label(&execution_id, MAX_EXECUTION_ID_BYTES)
            .map_err(|_| AttemptContextError::InvalidExecutionId)?;
        if capacity == 0 || capacity > MAX_CHANNEL_CAPACITY {
            return Err(AttemptContextError::InvalidCapacity);
        }
        let mut nonce = [0u8; 16];
        getrandom::getrandom(&mut nonce).map_err(|_| AttemptContextError::EntropyUnavailable)?;
        let (sender, receiver) = sync_channel(capacity);
        Ok((
            Self {
                shared: Arc::new(SharedAttemptContext {
                    execution_id,
                    nonce,
                    sender,
                    next_ordinal: AtomicU64::new(0),
                    dropped: AtomicU64::new(0),
                    single_attempt_id: None,
                    qualified_response: OnceLock::new(),
                }),
                cancellation: Arc::new(AttemptCancellation {
                    reason: AtomicU8::new(CANCELLED),
                    parent: None,
                }),
            },
            AttemptReceiver { receiver },
        ))
    }

    /// Bind one retry-free transport invocation to an already persisted attempt
    /// identity. This is correlation, not a credential or dispatch permit. Reuse
    /// is refused before constructing/polling another transport future.
    pub fn single_attempt(
        execution_id: impl Into<String>,
        attempt_id: impl Into<String>,
    ) -> Result<(Self, AttemptReceiver), AttemptContextError> {
        let attempt_id = attempt_id.into();
        if attempt_id.len() != 64
            || !attempt_id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(AttemptContextError::InvalidAttemptId);
        }
        let (mut context, receiver) = Self::channel(execution_id, 2)?;
        Arc::get_mut(&mut context.shared)
            .expect("new private context")
            .single_attempt_id = Some(attempt_id);
        Ok((context, receiver))
    }

    pub(crate) fn begin_checked(
        &self,
        provider: &str,
        model: Option<&str>,
    ) -> Result<Option<AttemptGuard>, ProviderError> {
        let guard = self.begin(provider, model);
        if guard.is_none() && self.shared.single_attempt_id.is_some() {
            return Err(ProviderError::InvalidRequest(
                "single physical-attempt context has already been consumed".into(),
            ));
        }
        if guard.is_none() {
            tracing::error!("physical-attempt ordinal exhausted; observation disabled for call");
        }
        Ok(guard)
    }

    /// Observations dropped because the bounded channel was full or disconnected.
    #[must_use]
    pub fn dropped_observations(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    /// Gateway dispatches observed by this context, distinct from authorized
    /// attempt slots or provider execution/billing. Counts do not authorize settlement.
    pub fn dispatch_count(&self) -> u64 {
        self.shared.next_ordinal.load(Ordering::Relaxed)
    }

    /// Source-qualified evidence survives decoder errors and stream drop. Only
    /// the built-in physical transport can attach it, once, to a tracked attempt.
    pub fn qualified_stream_evidence(&self) -> Option<crate::raw::QualifiedStreamEvidence> {
        match self.shared.qualified_response.get()? {
            QualifiedResponseEvidence::Stream(observed) => Some(observed.evidence()),
            QualifiedResponseEvidence::Buffered(_) => None,
        }
    }

    pub fn qualified_buffered_evidence(&self) -> Option<QualifiedBufferedEvidence> {
        match self.shared.qualified_response.get()? {
            QualifiedResponseEvidence::Buffered(observed) => Some(observed.as_ref().clone()),
            QualifiedResponseEvidence::Stream(_) => None,
        }
    }

    pub(crate) fn attach_qualified_stream(
        &self,
        observation: crate::raw::QualifiedStreamObservation,
    ) {
        if self.shared.single_attempt_id.is_some() {
            let _ = self
                .shared
                .qualified_response
                .set(QualifiedResponseEvidence::Stream(observation));
        }
    }

    /// Give one retry/setup future an independent cancellation reason while retaining execution
    /// identity and the shared monotonic attempt ordinal.
    pub(crate) fn fresh_call(&self) -> Self {
        Self {
            shared: self.shared.clone(),
            cancellation: Arc::new(AttemptCancellation {
                reason: AtomicU8::new(CANCELLED),
                parent: Some(self.cancellation.clone()),
            }),
        }
    }

    pub(crate) fn mark_timeout(&self) {
        self.cancellation.reason.store(TIMED_OUT, Ordering::Release);
    }

    fn timed_out(&self) -> bool {
        let mut scope = Some(self.cancellation.as_ref());
        while let Some(current) = scope {
            if current.reason.load(Ordering::Acquire) == TIMED_OUT {
                return true;
            }
            scope = current.parent.as_deref();
        }
        false
    }

    pub(crate) fn begin(&self, provider: &str, model: Option<&str>) -> Option<AttemptGuard> {
        let ordinal = if self.shared.single_attempt_id.is_some() {
            self.shared
                .next_ordinal
                .compare_exchange(0, 1, Ordering::Relaxed, Ordering::Relaxed)
                .ok()?;
            1
        } else {
            self.shared
                .next_ordinal
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                    current.checked_add(1)
                })
                .ok()?
                .saturating_add(1)
        };
        let provider = bounded_label(provider, MAX_PROVIDER_BYTES);
        let model = model.map(|value| bounded_label(value, MAX_MODEL_BYTES));
        let attempt_id = self
            .shared
            .single_attempt_id
            .clone()
            .unwrap_or_else(|| format!("att_{}_{ordinal:016x}", hex(&self.shared.nonce)));
        let guard = AttemptGuard {
            context: self.clone(),
            attempt_id,
            ordinal,
            provider,
            model,
            terminal: false,
            running_usage: None,
            response_facts: AttemptResponseFacts::default(),
        };
        guard.emit(AttemptPhase::Dispatch);
        Some(guard)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptContextError {
    InvalidExecutionId,
    InvalidAttemptId,
    InvalidCapacity,
    EntropyUnavailable,
}

impl fmt::Display for AttemptContextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidExecutionId => write!(f, "invalid attempt execution id"),
            Self::InvalidAttemptId => write!(f, "invalid persisted attempt id"),
            Self::InvalidCapacity => write!(f, "attempt channel capacity must be 1..=65536"),
            Self::EntropyUnavailable => write!(f, "attempt id entropy unavailable"),
        }
    }
}

impl std::error::Error for AttemptContextError {}

const CANCELLED: u8 = 0;
const TIMED_OUT: u8 = 1;

pub(crate) struct AttemptGuard {
    context: AttemptContext,
    attempt_id: String,
    ordinal: u64,
    provider: String,
    model: Option<String>,
    terminal: bool,
    running_usage: Option<ParsedUsage>,
    response_facts: AttemptResponseFacts,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct AttemptResponseFacts(
    Arc<OnceLock<ResponseFactValues>>,
    Arc<OnceLock<sandhi_core::UsageConventionV1>>,
);

#[derive(Debug, Clone, Default)]
struct ResponseFactValues {
    provider_status: Option<u16>,
    provider_request_id: Option<String>,
}

impl AttemptResponseFacts {
    pub(crate) fn record_usage_convention(&self, value: Option<sandhi_core::UsageConventionV1>) {
        if let Some(value) = value {
            let _ = self.1.set(value);
        }
    }
    pub(crate) fn record_headers(
        &self,
        status: u16,
        headers: &http::HeaderMap,
        vendor_request_id_header: Option<&str>,
    ) {
        self.record_peer_headers(
            sandhi_core::PeerRoleV1::Provider,
            status,
            headers,
            vendor_request_id_header,
        );
    }
    pub(crate) fn record_peer_headers(
        &self,
        role: sandhi_core::PeerRoleV1,
        status: u16,
        headers: &http::HeaderMap,
        vendor_request_id_header: Option<&str>,
    ) {
        let _ = self.0.set(ResponseFactValues {
            provider_status: Some(status),
            provider_request_id: crate::peer::response_request_id(
                role,
                headers,
                vendor_request_id_header,
            ),
        });
    }

    fn values(&self) -> ResponseFactValues {
        self.0.get().cloned().unwrap_or_default()
    }
}

impl AttemptGuard {
    fn emit(&self, phase: AttemptPhase) {
        let observation = AttemptObservation {
            version: ATTEMPT_OBSERVATION_VERSION,
            execution_id: self.context.shared.execution_id.clone(),
            attempt_id: self.attempt_id.clone(),
            attempt_ordinal: self.ordinal,
            provider: self.provider.clone(),
            model: self.model.clone(),
            observed_at_unix_ms: now_unix_ms(),
            phase,
        };
        if matches!(
            self.context.shared.sender.try_send(observation),
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_))
        ) {
            let _ = self.context.shared.dropped.try_update(
                Ordering::Relaxed,
                Ordering::Relaxed,
                |current| Some(current.saturating_add(1)),
            );
        }
    }

    pub(crate) fn set_response_facts(&mut self, facts: &AttemptResponseFacts) {
        self.response_facts = facts.clone();
    }

    pub(crate) fn cancellation_outcome(&self) -> AttemptOutcome {
        if self.context.timed_out() {
            AttemptOutcome::Timeout
        } else {
            AttemptOutcome::Cancelled
        }
    }

    /// Complete the opt-in stream with its canonical measurement completeness.
    /// Delivery failure and measurement completeness remain independent facts.
    pub(crate) fn finish_qualified_stream(
        &mut self,
        outcome: AttemptOutcome,
        usage: Option<&sandhi_core::UsageV2>,
    ) {
        let (status, request_id) = self.response_values();
        self.finish(
            outcome,
            status,
            request_id,
            usage.map(Into::into),
            usage.map_or(UsageCompleteness::Unavailable, |usage| usage.completeness),
        );
    }

    fn response_values(&self) -> (Option<u16>, Option<String>) {
        let facts = self.response_facts.values();
        let request_id = facts
            .provider_request_id
            .as_deref()
            .map(|id| bounded_label(id, MAX_PROVIDER_REQUEST_ID_BYTES));
        (facts.provider_status, request_id)
    }

    fn finish_success(&mut self, usage: Option<ParsedUsage>, completeness: UsageCompleteness) {
        let (provider_status, provider_request_id) = self.response_values();
        self.finish(
            AttemptOutcome::Success,
            provider_status,
            provider_request_id,
            usage,
            completeness,
        );
    }

    pub(crate) fn finish_error(&mut self, error: &ProviderError) {
        let (outcome, status, request_id) = match error {
            ProviderError::Auth => (AttemptOutcome::ProviderRejected, None, None),
            ProviderError::RateLimited => (AttemptOutcome::ProviderRejected, Some(429), None),
            ProviderError::GatewayFailure(failure) => (
                AttemptOutcome::ProviderRejected,
                failure.http_status,
                failure.request_id.clone(),
            ),
            ProviderError::Upstream {
                status, request_id, ..
            } => (
                AttemptOutcome::ProviderRejected,
                Some(*status),
                request_id
                    .as_deref()
                    .map(|id| bounded_label(id, MAX_PROVIDER_REQUEST_ID_BYTES)),
            ),
            ProviderError::Timeout(_) => (AttemptOutcome::Timeout, None, None),
            ProviderError::InvalidRequest(_)
            | ProviderError::OutputProjection(_)
            | ProviderError::Transport(_)
            | ProviderError::CircuitOpen => (AttemptOutcome::TransportError, None, None),
        };
        let completeness = if self.running_usage.is_some() {
            UsageCompleteness::Partial
        } else {
            UsageCompleteness::Unavailable
        };
        let (observed_status, observed_request_id) = self.response_values();
        let status = status.or(observed_status);
        let request_id = request_id.or(observed_request_id);
        self.finish(
            outcome,
            status,
            request_id,
            self.running_usage,
            completeness,
        );
    }

    fn finish(
        &mut self,
        outcome: AttemptOutcome,
        provider_status: Option<u16>,
        provider_request_id: Option<String>,
        usage: Option<ParsedUsage>,
        usage_completeness: UsageCompleteness,
    ) {
        if self.terminal {
            return;
        }
        self.terminal = true;
        self.emit(AttemptPhase::Terminal {
            outcome,
            provider_status,
            provider_request_id,
            usage,
            usage_completeness,
        });
    }

    pub(crate) fn wrap_stream(self, stream: ByteStream) -> ByteStream {
        Box::pin(ObservedStream {
            inner: stream,
            guard: Some(self),
        })
    }
}

impl Drop for AttemptGuard {
    fn drop(&mut self) {
        if self.terminal {
            return;
        }
        let outcome = if self.context.timed_out() {
            AttemptOutcome::Timeout
        } else {
            AttemptOutcome::Cancelled
        };
        let completeness = if self.running_usage.is_some() {
            UsageCompleteness::Partial
        } else {
            UsageCompleteness::Unavailable
        };
        let (provider_status, provider_request_id) = self.response_values();
        self.finish(
            outcome,
            provider_status,
            provider_request_id,
            self.running_usage,
            completeness,
        );
    }
}

/// Observe a non-streaming adapter send. Validation must happen before calling this function so a
/// rejected request never becomes a physical attempt.
pub(crate) async fn observe_value<T, Make, F>(
    context: Option<AttemptContext>,
    provider: &str,
    model: Option<&str>,
    make_future: Make,
) -> Result<T, ProviderError>
where
    Make: FnOnce(AttemptResponseFacts) -> F,
    F: std::future::Future<Output = Result<(T, Option<ParsedUsage>), ProviderError>>,
{
    let facts = AttemptResponseFacts::default();
    let guard = context
        .as_ref()
        .map(|context| context.begin_checked(provider, model))
        .transpose()?
        .flatten();
    let future = make_future(facts.clone());
    let Some(mut guard) = guard else {
        return future.await.map(|(response, _)| response);
    };
    guard.set_response_facts(&facts);
    match future.await {
        Ok((response, usage)) => {
            guard.finish_success(
                usage,
                if usage.is_some() {
                    UsageCompleteness::Final
                } else {
                    UsageCompleteness::Unavailable
                },
            );
            Ok(response)
        }
        Err(error) => {
            guard.finish_error(&error);
            Err(error)
        }
    }
}

/// Provider-response specialization of [`observe_value`].
pub(crate) async fn observe_complete<Make, F>(
    context: Option<AttemptContext>,
    provider: &str,
    model: Option<&str>,
    make_future: Make,
) -> Result<ProviderResponse, ProviderError>
where
    Make: FnOnce(AttemptResponseFacts) -> F,
    F: std::future::Future<Output = Result<(ProviderResponse, Option<ParsedUsage>), ProviderError>>,
{
    observe_value(context, provider, model, make_future).await
}

/// Qualify one built-in JSON response at its physical boundary, before a typed
/// codec can refuse it. Legacy calls/channels retain their existing behavior.
pub(crate) async fn observe_complete_qualified<Make, F>(
    context: Option<AttemptContext>,
    provider: &str,
    model: Option<&str>,
    endpoint: sandhi_core::EndpointFamilyV1,
    profile: sandhi_core::usage::AnthropicUsageProfile,
    make_future: Make,
) -> Result<ProviderResponse, ProviderError>
where
    Make: FnOnce(AttemptResponseFacts) -> F,
    F: std::future::Future<Output = Result<(ProviderResponse, Option<ParsedUsage>), ProviderError>>,
{
    use sandhi_core::{usage::AnthropicUsageProfile as A, EndpointFamilyV1 as E};
    if !matches!(
        endpoint,
        E::OpenaiChatCompletions
            | E::OpenaiResponses
            | E::AnthropicMessages
            | E::GeminiGenerateContent
    ) || (profile != A::SeparateCacheWrites && endpoint != E::AnthropicMessages)
    {
        return Err(ProviderError::InvalidRequest(
            "buffered usage profile does not match endpoint".into(),
        ));
    }
    let tracked = context
        .clone()
        .filter(|context| context.shared.single_attempt_id.is_some());
    observe_value(context, provider, model, move |facts| {
        let response_facts = facts.clone();
        let future = make_future(facts);
        async move {
            let (response, usage) = future.await?;
            if let Some(context) = tracked {
                let facts = response_facts.values();
                let request_id = facts.provider_request_id.map(|id| bounded_label(&id, 256));
                let counts = if facts.provider_status != Some(response.status)
                    || !(200..300).contains(&response.status)
                {
                    Err(sandhi_core::usage::BufferedUsageError::InvalidResponse)
                } else {
                    sandhi_core::usage::qualify_buffered_usage_with_profile(
                        endpoint,
                        response
                            .usage_convention
                            .map_or(profile, |value| value.anthropic_profile()),
                        &response.body,
                    )
                }
                .map(|mut usage| {
                    usage.upstream_request_id = request_id.clone();
                    usage
                });
                let _ = context
                    .shared
                    .qualified_response
                    .set(QualifiedResponseEvidence::Buffered(Box::new(
                        QualifiedBufferedEvidence {
                            endpoint,
                            upstream_request_id: request_id,
                            usage: counts,
                        },
                    )));
            }
            Ok((response, usage))
        }
    })
    .await
}

/// Observe stream setup and retain the lifecycle guard until the stream terminates or is dropped.
pub(crate) async fn observe_stream<Make, F>(
    context: Option<AttemptContext>,
    provider: &str,
    model: Option<&str>,
    make_future: Make,
) -> Result<ByteStream, ProviderError>
where
    Make: FnOnce(AttemptResponseFacts) -> F,
    F: std::future::Future<Output = Result<ByteStream, ProviderError>>,
{
    let facts = AttemptResponseFacts::default();
    let guard = context
        .as_ref()
        .map(|context| context.begin_checked(provider, model))
        .transpose()?
        .flatten();
    let future = make_future(facts.clone());
    let Some(mut guard) = guard else {
        return future.await;
    };
    guard.set_response_facts(&facts);
    match future.await {
        Ok(stream) => Ok(guard.wrap_stream(stream)),
        Err(error) => {
            guard.finish_error(&error);
            Err(error)
        }
    }
}

struct ObservedStream {
    inner: ByteStream,
    guard: Option<AttemptGuard>,
}

/// One guard at the actual HTTP response boundary; usage comes from the strict
/// complete-event observer, independently of the typed decoder or diagnostics.
pub(crate) async fn observe_qualified_stream<Make, F>(
    context: Option<AttemptContext>,
    provider: &str,
    model: Option<&str>,
    endpoint: sandhi_core::EndpointFamilyV1,
    profile: sandhi_core::usage::OpenAiStreamUsageProfile,
    anthropic_profile: sandhi_core::usage::AnthropicUsageProfile,
    make_future: Make,
) -> Result<crate::QualifiedProviderStream, ProviderError>
where
    Make: FnOnce(AttemptResponseFacts) -> F,
    F: std::future::Future<
        Output = Result<(crate::raw::RawChunkStream, Option<String>), ProviderError>,
    >,
{
    // Validate conventions before creating an attempt or performing HTTP I/O.
    let observer =
        crate::stream_usage::ProtocolStreamUsageObserver::for_endpoint_with_usage_profiles(
            endpoint,
            profile,
            anthropic_profile,
        )
        .map_err(|_| {
            ProviderError::InvalidRequest("stream usage profile does not match endpoint".into())
        })?;
    let facts = AttemptResponseFacts::default();
    let mut guard = context
        .as_ref()
        .map(|context| context.begin_checked(provider, model))
        .transpose()?
        .flatten();
    if let Some(guard) = &mut guard {
        guard.set_response_facts(&facts);
    }
    let (raw, request_id) = match make_future(facts.clone()).await {
        Ok(response) => response,
        Err(error) => {
            if let Some(guard) = &mut guard {
                guard.finish_error(&error);
            }
            return Err(error);
        }
    };
    let observer = if let Some(convention) = facts.1.get() {
        crate::stream_usage::ProtocolStreamUsageObserver::for_endpoint_with_usage_profiles(
            endpoint,
            convention.chat_profile(),
            convention.anthropic_profile(),
        )
        .map_err(|_| ProviderError::OutputProjection("invalid gateway usage convention".into()))?
        .with_gateway_peer()
    } else {
        observer
    };
    let (raw, observation) =
        crate::raw::qualified_stream::observe_response(raw, guard, observer, request_id);
    if let Some(context) = context {
        context.attach_qualified_stream(observation.clone());
    }
    use futures_util::StreamExt;
    let metadata = observation.clone();
    Ok(crate::QualifiedProviderStream {
        stream: Box::pin(raw.map(move |chunk| {
            chunk.map(|data| StreamChunk {
                data,
                usage: None,
                usage_running: None,
                cache_read_observation: metadata.cache_read_observation(),
                attempts: 1,
                terminal: false,
            })
        })),
        observation,
    })
}

impl Stream for ObservedStream {
    type Item = Result<StreamChunk, ProviderError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                if let Some(guard) = self.guard.as_mut() {
                    if let Some(usage) = chunk.usage_running {
                        guard.running_usage = Some(usage);
                    }
                    if chunk.terminal {
                        guard.finish_success(
                            chunk.usage,
                            if chunk.usage.is_some() {
                                UsageCompleteness::Final
                            } else {
                                UsageCompleteness::Unavailable
                            },
                        );
                    }
                }
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(error))) => {
                if let Some(mut guard) = self.guard.take() {
                    guard.finish_error(&error);
                }
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                if let Some(mut guard) = self.guard.take() {
                    let completeness = if guard.running_usage.is_some() {
                        UsageCompleteness::Partial
                    } else {
                        UsageCompleteness::Unavailable
                    };
                    let (provider_status, provider_request_id) = guard.response_values();
                    guard.finish(
                        AttemptOutcome::IncompleteStream,
                        provider_status,
                        provider_request_id,
                        guard.running_usage,
                        completeness,
                    );
                }
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

fn validate_label(value: &str, max_bytes: usize) -> Result<(), ()> {
    if value.is_empty()
        || value.len() > max_bytes
        || value.chars().any(|character| character.is_control())
    {
        return Err(());
    }
    Ok(())
}

pub(crate) fn bounded_label(value: &str, max_bytes: usize) -> String {
    let mut output = String::with_capacity(value.len().min(max_bytes));
    for character in value.chars() {
        let sanitized = if character.is_control() {
            '\u{fffd}'
        } else {
            character
        };
        if output.len() + sanitized.len_utf8() > max_bytes {
            break;
        }
        output.push(sanitized);
    }
    output
}

fn now_unix_ms() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(millis).unwrap_or(i64::MAX)
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(DIGITS[usize::from(byte >> 4)]));
        output.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{stream, StreamExt};

    fn context() -> (AttemptContext, AttemptReceiver) {
        AttemptContext::channel("exec_opaque", 16).unwrap()
    }

    #[tokio::test]
    async fn single_attempt_reuse_refuses_before_constructing_any_transport_future() {
        let id = "a".repeat(64);
        let (context, receiver) = AttemptContext::single_attempt("execution", &id).unwrap();
        observe_value(Some(context.fresh_call()), "provider", None, |_| async {
            Ok(((), None))
        })
        .await
        .unwrap();
        let result = observe_value(Some(context.fresh_call()), "provider", None, |_| {
            panic!("second transport future must never be constructed");
            #[allow(unreachable_code)]
            async {
                Ok(((), None))
            }
        })
        .await;
        assert!(matches!(result, Err(ProviderError::InvalidRequest(_))));
        let result = observe_stream(Some(context.fresh_call()), "provider", None, |_| {
            panic!("stream future must never be constructed after consuming a unary attempt");
            #[allow(unreachable_code)]
            async {
                Ok(Box::pin(stream::empty()) as ByteStream)
            }
        })
        .await;
        assert!(matches!(result, Err(ProviderError::InvalidRequest(_))));
        let events = receiver.drain();
        assert_eq!(events.len(), 2);
        assert!(events
            .iter()
            .all(|e| e.attempt_id == id && e.attempt_ordinal == 1));
        assert_eq!(context.dropped_observations(), 0);
        assert!(AttemptContext::single_attempt("exec", "invalid").is_err());
    }

    #[tokio::test]
    async fn complete_emits_dispatch_and_bounded_terminal_measurement() {
        let (context, receiver) = context();
        let usage = ParsedUsage {
            cache_read_observation: None,
            tokens_in: 2,
            tokens_out: 3,
            ..ParsedUsage::default()
        };
        let response = ProviderResponse {
            usage_convention: None,
            status: 200,
            body: serde_json::json!({}),
            usage,
            attempts: 1,
        };
        observe_complete(Some(context), "provider", Some("model"), |_| async {
            Ok((response, Some(usage)))
        })
        .await
        .unwrap();

        let observations = receiver.drain();
        assert_eq!(observations.len(), 2);
        assert!(matches!(observations[0].phase, AttemptPhase::Dispatch));
        assert!(matches!(
            observations[1].phase,
            AttemptPhase::Terminal {
                outcome: AttemptOutcome::Success,
                usage: Some(value),
                usage_completeness: UsageCompleteness::Final,
                ..
            } if value == usage
        ));
    }

    #[tokio::test]
    async fn dropped_stream_records_partial_cancellation_once() {
        let (context, receiver) = context();
        let partial = ParsedUsage {
            cache_read_observation: None,
            tokens_in: 7,
            ..ParsedUsage::default()
        };
        let inner: ByteStream = Box::pin(stream::iter(vec![Ok(StreamChunk {
            cache_read_observation: None,
            data: bytes::Bytes::from_static(b"data"),
            usage: None,
            usage_running: Some(partial),
            attempts: 1,
            terminal: false,
        })]));
        let mut observed = observe_stream(Some(context), "provider", Some("model"), |_| async {
            Ok(inner)
        })
        .await
        .unwrap();
        observed.next().await.unwrap().unwrap();
        drop(observed);

        let observations = receiver.drain();
        assert_eq!(observations.len(), 2);
        assert!(matches!(
            observations[1].phase,
            AttemptPhase::Terminal {
                outcome: AttemptOutcome::Cancelled,
                usage: Some(value),
                usage_completeness: UsageCompleteness::Partial,
                ..
            } if value == partial
        ));
    }

    #[tokio::test]
    async fn post_header_cancellation_preserves_provider_correlation() {
        let (context, receiver) = context();
        let inner: ByteStream = Box::pin(stream::pending());
        let observed = observe_stream(
            Some(context),
            "provider",
            Some("model"),
            |facts| async move {
                let mut headers = http::HeaderMap::new();
                headers.insert("x-request-id", "provider-stream-9".parse().unwrap());
                facts.record_headers(200, &headers, None);
                Ok(inner)
            },
        )
        .await
        .unwrap();
        drop(observed);

        let observations = receiver.drain();
        assert!(matches!(
            &observations[1].phase,
            AttemptPhase::Terminal {
                outcome: AttemptOutcome::Cancelled,
                provider_status: Some(200),
                provider_request_id: Some(id),
                ..
            } if id == "provider-stream-9"
        ));
    }

    #[tokio::test]
    async fn unary_timeout_after_headers_preserves_provider_correlation() {
        let (context, receiver) = context();
        let recorded = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let recorded_in_future = recorded.clone();
        let timeout_context = context.clone();
        let task = tokio::spawn(observe_complete(
            Some(context),
            "provider",
            Some("model"),
            move |facts| async move {
                let mut headers = http::HeaderMap::new();
                headers.insert("x-request-id", "provider-unary-11".parse().unwrap());
                facts.record_headers(200, &headers, None);
                recorded_in_future.store(true, Ordering::Release);
                std::future::pending::<
                    Result<(ProviderResponse, Option<ParsedUsage>), ProviderError>,
                >()
                .await
            },
        ));
        while !recorded.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        timeout_context.mark_timeout();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());

        let observations = receiver.drain();
        assert!(matches!(
            &observations[1].phase,
            AttemptPhase::Terminal {
                outcome: AttemptOutcome::Timeout,
                provider_status: Some(200),
                provider_request_id: Some(id),
                ..
            } if id == "provider-unary-11"
        ));
    }

    #[tokio::test]
    async fn incomplete_eof_preserves_provider_correlation() {
        let (context, receiver) = context();
        let inner: ByteStream = Box::pin(stream::empty());
        let mut observed = observe_stream(
            Some(context),
            "provider",
            Some("model"),
            |facts| async move {
                let mut headers = http::HeaderMap::new();
                headers.insert("x-request-id", "provider-eof-10".parse().unwrap());
                facts.record_headers(200, &headers, None);
                Ok(inner)
            },
        )
        .await
        .unwrap();
        assert!(observed.next().await.is_none());

        let observations = receiver.drain();
        assert!(matches!(
            &observations[1].phase,
            AttemptPhase::Terminal {
                outcome: AttemptOutcome::IncompleteStream,
                provider_status: Some(200),
                provider_request_id: Some(id),
                ..
            } if id == "provider-eof-10"
        ));
    }

    #[tokio::test]
    async fn empty_nonterminal_chunk_does_not_hide_later_transport_error() {
        let (context, receiver) = context();
        let inner: ByteStream = Box::pin(stream::iter(vec![
            Ok(StreamChunk {
                cache_read_observation: None,
                data: bytes::Bytes::new(),
                usage: None,
                usage_running: None,
                attempts: 1,
                terminal: false,
            }),
            Err(ProviderError::Transport("after-empty".into())),
        ]));
        let mut observed = observe_stream(Some(context), "provider", Some("model"), |_| async {
            Ok(inner)
        })
        .await
        .unwrap();
        observed.next().await.unwrap().unwrap();
        assert!(matches!(
            observed.next().await,
            Some(Err(ProviderError::Transport(_)))
        ));

        let observations = receiver.drain();
        assert!(matches!(
            observations[1].phase,
            AttemptPhase::Terminal {
                outcome: AttemptOutcome::TransportError,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn a_terminal_marker_is_emitted_at_most_once_even_if_the_inner_stream_misbehaves() {
        let (context, receiver) = context();
        let inner: ByteStream = Box::pin(stream::iter(vec![
            Ok(StreamChunk {
                cache_read_observation: None,
                data: bytes::Bytes::new(),
                usage: None,
                usage_running: None,
                attempts: 1,
                terminal: true,
            }),
            Err(ProviderError::Transport("after-terminal".into())),
        ]));
        let mut observed = observe_stream(Some(context), "provider", Some("model"), |_| async {
            Ok(inner)
        })
        .await
        .unwrap();
        observed.next().await.unwrap().unwrap();
        assert!(observed.next().await.unwrap().is_err());

        let observations = receiver.drain();
        assert_eq!(
            observations
                .iter()
                .filter(|item| matches!(item.phase, AttemptPhase::Terminal { .. }))
                .count(),
            1
        );
        assert!(matches!(
            observations[1].phase,
            AttemptPhase::Terminal {
                outcome: AttemptOutcome::Success,
                ..
            }
        ));
    }

    #[test]
    fn separate_contexts_include_collision_resistant_random_nonces() {
        let (first, first_receiver) = AttemptContext::channel("same-execution", 2).unwrap();
        let (second, second_receiver) = AttemptContext::channel("same-execution", 2).unwrap();
        drop(first.begin("provider", Some("model")).unwrap());
        drop(second.begin("provider", Some("model")).unwrap());

        assert_ne!(
            first_receiver.drain()[0].attempt_id,
            second_receiver.drain()[0].attempt_id
        );
    }

    #[test]
    fn context_rejects_log_injection_and_observer_is_bounded() {
        assert!(AttemptContext::channel("bad\nvalue", 1).is_err());
        assert!(AttemptContext::channel("exec", 0).is_err());
        let (context, receiver) = AttemptContext::channel("exec", 1).unwrap();
        drop(context.begin("provider", Some("model")).unwrap());
        assert_eq!(receiver.drain().len(), 1);
        assert_eq!(context.dropped_observations(), 1);
    }

    #[test]
    fn sanitized_labels_never_exceed_their_utf8_byte_budget() {
        let controls = "\0".repeat(1_000);
        let sanitized = bounded_label(&controls, MAX_PROVIDER_BYTES);
        assert!(sanitized.len() <= MAX_PROVIDER_BYTES);

        let boundary = format!("{}\0", "a".repeat(MAX_PROVIDER_BYTES - 2));
        let sanitized = bounded_label(&boundary, MAX_PROVIDER_BYTES);
        assert!(sanitized.len() <= MAX_PROVIDER_BYTES);
        assert!(sanitized.is_char_boundary(sanitized.len()));
    }

    #[test]
    fn timeout_lineage_reaches_descendants_without_contaminating_siblings() {
        let (root, receiver) = context();
        let timed_out_parent = root.fresh_call();
        let timed_out_child = timed_out_parent.fresh_call();
        let unaffected_sibling = root.fresh_call();
        timed_out_parent.mark_timeout();
        drop(timed_out_parent);

        drop(timed_out_child.begin("provider", Some("child")).unwrap());
        drop(
            unaffected_sibling
                .begin("provider", Some("sibling"))
                .unwrap(),
        );

        let terminals: Vec<_> = receiver
            .drain()
            .into_iter()
            .filter_map(|observation| match observation.phase {
                AttemptPhase::Terminal { outcome, .. } => Some(outcome),
                AttemptPhase::Dispatch => None,
            })
            .collect();
        assert_eq!(
            terminals,
            vec![AttemptOutcome::Timeout, AttemptOutcome::Cancelled]
        );
    }

    #[test]
    fn ordinal_exhaustion_disables_observation_without_wrapping() {
        let (context, receiver) = context();
        context
            .shared
            .next_ordinal
            .store(u64::MAX, Ordering::Relaxed);
        assert!(context.begin("provider", Some("model")).is_none());
        assert!(receiver.drain().is_empty());
    }

    #[tokio::test]
    async fn invalid_stream_profiles_refuse_before_attempt_authorization_or_http_setup() {
        use sandhi_core::usage::{AnthropicUsageProfile as A, OpenAiStreamUsageProfile as O};
        use sandhi_core::EndpointFamilyV1 as E;
        for (endpoint, openai, anthropic) in [
            (E::OpenaiChatCompletions, O::Standard, A::ImplicitCache),
            (
                E::AnthropicMessages,
                O::TerminalChunk,
                A::SeparateCacheWrites,
            ),
            (E::CohereChat, O::Standard, A::SeparateCacheWrites),
        ] {
            let (context, receiver) =
                AttemptContext::single_attempt("invalid-profile", "e".repeat(64)).unwrap();
            let result = observe_qualified_stream(
                Some(context.clone()),
                "fixture",
                Some("fixture"),
                endpoint,
                openai,
                anthropic,
                |_| async {
                    panic!("invalid profile reached physical HTTP setup");
                    #[allow(unreachable_code)]
                    Ok((
                        Box::pin(futures_util::stream::empty()) as crate::raw::RawChunkStream,
                        None,
                    ))
                },
            )
            .await;
            assert!(matches!(result, Err(ProviderError::InvalidRequest(_))));
            assert_eq!(context.dispatch_count(), 0);
            assert!(receiver.drain().is_empty());
        }
    }

    #[tokio::test]
    async fn invalid_buffered_profiles_refuse_before_authorization_and_http_setup() {
        use sandhi_core::{usage::AnthropicUsageProfile as A, EndpointFamilyV1 as E};
        for (endpoint, profile) in [
            (E::OpenaiChatCompletions, A::ImplicitCache),
            (E::CohereChat, A::SeparateCacheWrites),
        ] {
            let (context, receiver) =
                AttemptContext::single_attempt("invalid-buffer-profile", "a".repeat(64)).unwrap();
            let result = observe_complete_qualified(
                Some(context.clone()),
                "fixture",
                Some("fixture"),
                endpoint,
                profile,
                |_| async {
                    panic!("invalid buffered profile reached HTTP setup");
                    #[allow(unreachable_code)]
                    Ok((
                        ProviderResponse {
                            usage_convention: None,
                            status: 200,
                            body: serde_json::json!({}),
                            usage: Default::default(),
                            attempts: 1,
                        },
                        None,
                    ))
                },
            )
            .await;
            assert!(matches!(result, Err(ProviderError::InvalidRequest(_))));
            assert_eq!(context.dispatch_count(), 0);
            assert!(context.qualified_buffered_evidence().is_none());
            assert!(receiver.drain().is_empty());
        }
    }

    #[tokio::test]
    async fn buffered_source_evidence_survives_diagnostic_loss_and_requires_response_facts() {
        use sandhi_core::{EndpointFamilyV1 as E, UsageCompleteness};
        for headers in [false, true] {
            let (context, receiver) =
                AttemptContext::single_attempt("buffered-facts", "b".repeat(64)).unwrap();
            drop(receiver);
            observe_complete_qualified(Some(context.clone()),"fixture",Some("fixture"),E::OpenaiChatCompletions,Default::default(),
                |facts|async move {
                    if headers {facts.record_headers(200,&Default::default(),None);}
                    Ok((ProviderResponse{usage_convention:None,status:200,body:serde_json::json!({"usage":{"prompt_tokens":8,"completion_tokens":4,
                        "prompt_tokens_details":{"cached_tokens":6},"completion_tokens_details":{"reasoning_tokens":0}}}),
                        usage:Default::default(),attempts:1},None))
                }).await.unwrap();
            let evidence = context.qualified_buffered_evidence().unwrap();
            assert!(context.qualified_stream_evidence().is_none());
            assert_eq!(evidence.usage.is_ok(), headers);
            if let Ok(usage) = evidence.usage {
                assert_eq!(
                    (usage.tokens_in, usage.cache_read_tokens, usage.tokens_out),
                    (2, 6, 4)
                );
                assert_eq!(usage.reasoning_tokens, Some(0));
                assert_eq!(usage.completeness, UsageCompleteness::Final);
            }
            assert_eq!(context.dispatch_count(), 1);
            assert_eq!(context.dropped_observations(), 2);
        }
    }
}
