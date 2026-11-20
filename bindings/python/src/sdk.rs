use super::{parse_headers_json, SandhiProviderError, TypedProvider};
use futures_util::StreamExt;
use pyo3::exceptions::{PyStopAsyncIteration, PyValueError};
use pyo3::prelude::*;
use reqwest::{header::HeaderMap, StatusCode};
use sandhi_core::native_references::request_references;
use sandhi_core::{
    chat::derive_session_id, policy::strict_json, ChatRequestV1, ChatStreamEventV1,
    NativeInputFeatureV1, PeerRoleV1, RequestMetadataV1,
};
use sandhi_dialect::{
    decode_request, encode_response_with_emission, IngressDialect, IngressError, TranslatedStream,
};
use sandhi_providers::{ProviderError, ProviderHandle};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::{
    sync::{mpsc, Mutex},
    task::JoinHandle,
};

pyo3::create_exception!(
    sandhi_gateway,
    SandhiSdkError,
    SandhiProviderError,
    "SDK call failed. The exception JSON contains status, the typed error, and the dialect body."
);

pub(super) struct Call {
    dialect: IngressDialect,
    handle: ProviderHandle,
    request: ChatRequestV1,
    headers: HeaderMap,
}

fn error_json(dialect: IngressDialect, error: IngressError) -> String {
    json!({
        "status": error.status().as_u16(),
        "error": error.payload(),
        "body": error.body(dialect),
    })
    .to_string()
}

fn invalid(dialect: IngressDialect, message: impl Into<String>) -> PyErr {
    SandhiSdkError::new_err(error_json(
        dialect,
        IngressError::invalid(StatusCode::BAD_REQUEST, message),
    ))
}

fn provider_error(
    dialect: IngressDialect,
    handle: &ProviderHandle,
    error: &ProviderError,
) -> String {
    error_json(
        dialect,
        IngressError::from_provider_redacted(error, handle.slug()),
    )
}

fn event_json(event: Option<&str>, data: Value) -> String {
    json!({"event": event, "data": data}).to_string()
}

impl Call {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        provider: &TypedProvider,
        dialect: &str,
        request_json: &str,
        model: Option<&str>,
        metadata_json: Option<&str>,
        wire_headers_json: Option<String>,
        stream: bool,
    ) -> PyResult<Self> {
        let dialect = IngressDialect::sdk_chat(dialect)
            .ok_or_else(|| PyValueError::new_err("unknown SDK chat dialect"))?;
        let body = strict_json(request_json.as_bytes())
            .map_err(|_| invalid(dialect, "body is not valid JSON"))?;
        let headers = parse_headers_json(wire_headers_json)?;
        let mut metadata: RequestMetadataV1 = metadata_json
            .map(|value| {
                strict_json(value.as_bytes())
                    .map_err(|_| invalid(dialect, "metadata is not valid JSON"))
                    .and_then(|value| {
                        if !value.is_object() {
                            return Err(invalid(dialect, "metadata must be RequestMetadataV1"));
                        }
                        serde_json::from_value(value)
                            .map_err(|_| invalid(dialect, "metadata must be RequestMetadataV1"))
                    })
            })
            .transpose()?
            .unwrap_or_default();
        let explicit_session = metadata.session_id.as_deref().or_else(|| {
            headers
                .get("x-sandhi-session")
                .and_then(|v| v.to_str().ok())
        });
        metadata.session_id = derive_session_id(explicit_session, &body);
        if dialect != IngressDialect::Gemini {
            if model.is_some() {
                return Err(invalid(dialect, "model context is only used by Gemini"));
            }
            if let Some(field) = body.get("stream").filter(|field| !field.is_null()) {
                if field.as_bool() != Some(stream) {
                    return Err(invalid(
                        dialect,
                        "request stream field disagrees with the SDK method",
                    ));
                }
            }
        }
        let (mut request, _) =
            decode_request(dialect, body, metadata).map_err(|message| invalid(dialect, message))?;
        if dialect == IngressDialect::Gemini {
            request.model = model
                .filter(|model| !model.trim().is_empty())
                .ok_or_else(|| invalid(dialect, "Gemini requires model context"))?
                .to_owned();
        }

        let handle = provider.handle.clone();
        request
            .native_input_for(handle.family().endpoint_family())
            .map_err(|message| invalid(dialect, message))?;
        if handle.peer_role() == PeerRoleV1::Provider
            && request.native_input.as_ref().is_some_and(|native| {
                native
                    .nonportable_features
                    .contains(&NativeInputFeatureV1::OpaqueState)
            })
        {
            return Err(invalid(
                dialect,
                "native continuation state requires a gateway with reference ownership",
            ));
        }
        request.include_native_response = dialect.family() == handle.family();
        request.include_native_stream =
            stream && request.include_native_response && handle.supports_native_stream();
        let render_error =
            |error| SandhiSdkError::new_err(provider_error(dialect, &handle, &error));
        if handle.peer_role() == PeerRoleV1::Provider {
            let effective = handle
                .encode_execution_request(&request)
                .map_err(render_error)?;
            let references = request_references(&effective, handle.family().endpoint_family())
                .map_err(|message| invalid(dialect, message))?;
            if !references.is_empty() {
                return Err(invalid(
                    dialect,
                    "native continuation state requires a gateway with reference ownership",
                ));
            }
        } else {
            handle.preflight_request(&request).map_err(render_error)?;
        }
        Ok(Self {
            dialect,
            handle,
            request,
            headers,
        })
    }

    pub(super) async fn complete(self) -> PyResult<String> {
        let Self {
            dialect,
            handle,
            request,
            headers,
        } = self;
        let response = handle
            .complete_with(request, headers)
            .await
            .map_err(|error| SandhiSdkError::new_err(provider_error(dialect, &handle, &error)))?;
        let (body, _) = encode_response_with_emission(dialect, &response).map_err(|message| {
            let error = ProviderError::OutputProjection(message.into());
            SandhiSdkError::new_err(provider_error(dialect, &handle, &error))
        })?;
        Ok(body.to_string())
    }

    pub(super) fn stream(self) -> SdkEventStream {
        let (tx, rx) = mpsc::channel(64);
        let task = pyo3_async_runtimes::tokio::get_runtime().spawn(async move {
            let dialect = self.dialect;
            let handle = self.handle.clone();
            if let Err(error) = self.deliver(&tx).await {
                let _ = tx.send(Err(provider_error(dialect, &handle, &error))).await;
            }
        });
        SdkEventStream {
            rx: Arc::new(Mutex::new(rx)),
            task,
        }
    }

    async fn deliver(self, tx: &mpsc::Sender<Result<String, String>>) -> Result<(), ProviderError> {
        let Self {
            dialect,
            handle,
            request,
            headers,
        } = self;
        let native = request.include_native_stream;
        let mut source = handle.stream_with(request, headers).await?;
        let mut projection = TranslatedStream::new(dialect, !native);
        let mut native_error_sent = false;
        while let Some(event) = source.next().await {
            let event = match event {
                Ok(event) => event,
                Err(_) if native_error_sent => return Ok(()),
                Err(error) => ChatStreamEventV1::Error {
                    error: error.as_typed(Some(handle.slug())),
                },
            };
            match event {
                ChatStreamEventV1::NativeFrame { frame } => {
                    if !native
                        || frame.endpoint_family != dialect.family().endpoint_family()
                        || frame.sse.len() > sandhi_core::MAX_NATIVE_STREAM_FRAME_BYTES
                    {
                        return Err(ProviderError::OutputProjection(
                            "incompatible native stream frame".into(),
                        ));
                    }
                    if let Some(data) = frame.json_data() {
                        native_error_sent |= sandhi_dialect::is_native_stream_error(&data);
                        let name = frame
                            .sse
                            .lines()
                            .filter_map(|line| line.strip_prefix("event:"))
                            .next_back()
                            .map(|value| value.strip_prefix(' ').unwrap_or(value));
                        if tx.send(Ok(event_json(name, data))).await.is_err() {
                            return Ok(());
                        }
                    }
                }
                ChatStreamEventV1::Error { error } => {
                    if !native_error_sent {
                        for (name, data) in projection
                            .push(&ChatStreamEventV1::Error { error })
                            .map_err(projection_error)?
                        {
                            if tx.send(Ok(event_json(name, data))).await.is_err() {
                                break;
                            }
                        }
                    }
                    return Ok(());
                }
                event if !native => {
                    for (name, data) in projection.push(&event).map_err(projection_error)? {
                        if tx.send(Ok(event_json(name, data))).await.is_err() {
                            return Ok(());
                        }
                    }
                }
                _ => {}
            }
        }
        if !native {
            for (name, data) in projection.finish().map_err(projection_error)? {
                if tx.send(Ok(event_json(name, data))).await.is_err() {
                    break;
                }
            }
        }
        Ok(())
    }
}

fn projection_error(message: &'static str) -> ProviderError {
    ProviderError::OutputProjection(message.into())
}

#[pyclass]
pub(super) struct SdkEventStream {
    rx: Arc<Mutex<mpsc::Receiver<Result<String, String>>>>,
    task: JoinHandle<()>,
}

impl Drop for SdkEventStream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[pymethods]
impl SdkEventStream {
    fn __aiter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    fn __anext__<'py>(slf: PyRef<'py, Self>) -> PyResult<Bound<'py, PyAny>> {
        let py = slf.py();
        let rx = slf.rx.clone();
        let keepalive: Py<Self> = slf.into();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            // anext() can be called on a temporary stream. Keep it alive
            // until this read completes so Drop cannot abort it prematurely.
            let _stream = keepalive;
            match rx.lock().await.recv().await {
                Some(Ok(event)) => Ok(event),
                Some(Err(error)) => Err(SandhiSdkError::new_err(error)),
                None => Err(PyStopAsyncIteration::new_err(())),
            }
        })
    }

    fn aclose<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        self.task.abort();
        let rx = self.rx.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let mut rx = rx.lock().await;
            rx.close();
            while rx.try_recv().is_ok() {}
            Ok(())
        })
    }
}
