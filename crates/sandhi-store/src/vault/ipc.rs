//! Bounded synchronous facade over a thread-owned async broker client (TD-0026 W04).
//! No runtime is created, entered or dropped on the caller's async executor. Sync callers
//! still block waiting for a reply; async hosts must offload the entire operation.

use super::{SentinelPassExactBinding, Vault, VaultCapabilities, VaultError, VaultReferencePlan};
use sentinelpass_protocol::{
    IpcClient, IpcMessage, Origin, ServiceOutcome, VaultOp, VaultOpResult,
};
use std::collections::HashMap;
use std::sync::mpsc::{self, SyncSender};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const QUEUE_CAPACITY: usize = 16;
const DEFAULT_TIMEOUT_MS: u64 = 5000;

struct Work {
    message: IpcMessage,
    deadline: Instant,
    reply: SyncSender<Result<IpcMessage, VaultError>>,
}

/// Native broker adapter. One worker and at most 16 queued requests; deadlines include queue
/// residence. Dropping the last sender closes the worker after bounded in-flight I/O; it never
/// joins or destroys a Tokio runtime on the caller thread. No automatic retries or unlocking.
pub struct SentinelPassIpcVault {
    client_id: String,
    sender: SyncSender<Work>,
    timeout: Duration,
    legacy_lookups: Mutex<HashMap<String, u64>>,
}

impl SentinelPassIpcVault {
    pub fn new() -> Result<Self, VaultError> {
        let client_id =
            std::env::var("SANDHI_SENTINELPASS_CLIENT_ID").unwrap_or_else(|_| "sandhi".into());
        let client_token = std::env::var("SENTINELPASS_CLIENT_TOKEN")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .ok_or(VaultError::Configuration)?;
        let socket = std::env::var("SANDHI_SENTINELPASS_SOCKET")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| sentinelpass_protocol::default_ipc_socket_path());
        let millis = std::env::var("SANDHI_SENTINELPASS_TIMEOUT_MS")
            .map(|v| v.parse::<u64>().map_err(|_| VaultError::Configuration))
            .unwrap_or(Ok(DEFAULT_TIMEOUT_MS))?;
        let token_file =
            std::env::var_os("SANDHI_SENTINELPASS_TOKEN_FILE").map(std::path::PathBuf::from);
        let client = client_with_token_file(socket, token_file.as_deref())?
            .with_context(Some(client_token), Some(Origin::Cli));
        Self::from_client(client_id, client, Duration::from_millis(millis))
    }

    fn from_client(
        client_id: String,
        client: IpcClient,
        timeout: Duration,
    ) -> Result<Self, VaultError> {
        if client_id.trim().is_empty()
            || !(Duration::from_millis(50)..=Duration::from_secs(30)).contains(&timeout)
        {
            return Err(VaultError::Configuration);
        }
        let (sender, receiver) = mpsc::sync_channel::<Work>(QUEUE_CAPACITY);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("sandhi-vault-ipc".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                let Ok(runtime) = runtime else {
                    let _ = ready_tx.send(Err(VaultError::Configuration));
                    return;
                };
                if ready_tx.send(Ok(())).is_err() {
                    return;
                }
                while let Ok(work) = receiver.recv() {
                    let result = if Instant::now() >= work.deadline {
                        Err(VaultError::Timeout)
                    } else {
                        runtime.block_on(async {
                            tokio::time::timeout_at(work.deadline.into(), client.send(work.message))
                                .await
                                .map_err(|_| VaultError::Timeout)?
                                // Do not relay arbitrary broker/parser error text, which may contain secrets.
                                .map_err(|_| {
                                    VaultError::Backend(
                                        "broker transport or protocol failure".into(),
                                    )
                                })
                        })
                    };
                    let _ = work.reply.send(result);
                }
            })
            .map_err(|_| VaultError::Configuration)?;
        ready_rx
            .recv_timeout(Duration::from_secs(1))
            .map_err(|_| VaultError::Configuration)??;
        Ok(Self {
            client_id,
            sender,
            timeout,
            legacy_lookups: Mutex::new(HashMap::new()),
        })
    }

    fn domain(provider: &str, label: &str) -> String {
        format!("sandhi:{provider}:{label}")
    }

    fn account(provider: &str, label: &str) -> String {
        format!("{provider}:{label}")
    }

    fn record_legacy_lookup(&self, provider: &str, label: &str) -> Result<u64, VaultError> {
        let mut lookups = self
            .legacy_lookups
            .lock()
            .map_err(|_| VaultError::Backend("legacy lookup accounting poisoned".into()))?;
        let count = lookups
            .entry(Self::account(provider, label))
            .and_modify(|count| *count += 1)
            .or_insert(1);
        if *count == 1 {
            tracing::warn!(
                backend = "sentinelpass-ipc",
                reference = %Self::account(provider, label),
                domain = %Self::domain(provider, label),
                "using deprecated SentinelPass legacy external-secret retrieval"
            );
        }
        Ok(*count)
    }

    fn is_strict_legacy_denial(error: &str) -> bool {
        // The protocol still returns this denial as display text. Match only the pinned
        // SentinelPass wording, never relay it, and never infer meaning from arbitrary errors.
        error.contains("strict profile")
            && error.contains("legacy external-secret grants are disabled")
            && error.contains("exact-entry service grants")
    }

    fn exact_secret_from_report(response: IpcMessage) -> Result<Option<String>, VaultError> {
        let IpcMessage::ServiceResult { outcome } = response else {
            return Err(VaultError::Backend(
                "broker returned the wrong exact-entry response type".into(),
            ));
        };
        let report = match outcome {
            ServiceOutcome::Ok {
                result: VaultOpResult::Report(report),
            } => report,
            ServiceOutcome::Ok { .. } => {
                return Err(VaultError::Backend(
                    "broker returned an invalid exact-entry result".into(),
                ));
            }
            ServiceOutcome::Err { error } => match error.code.as_str() {
                "locked" => return Err(VaultError::Locked),
                "not_found" => return Ok(None),
                "denied" => return Err(VaultError::Denied),
                _ => {
                    return Err(VaultError::Backend(
                        "broker exact-entry request failed".into(),
                    ))
                }
            },
        };
        match report.get("status").and_then(|value| value.as_str()) {
            Some("authorized") => report
                .get("value")
                .and_then(|value| value.as_str())
                .map(str::to_owned)
                .map(Some)
                .ok_or_else(|| {
                    VaultError::Backend("broker exact-entry response omitted its value".into())
                }),
            Some("not_found") => Ok(None),
            Some("locked") => Err(VaultError::Locked),
            Some("denied") => Err(VaultError::Denied),
            _ => Err(VaultError::Backend(
                "broker returned an invalid exact-entry status".into(),
            )),
        }
    }

    fn get_exact_secret_with_token(
        &self,
        binding: &SentinelPassExactBinding,
        token: &str,
    ) -> Result<Option<String>, VaultError> {
        super::validate_exact_binding(binding.entry_id, &binding.token_env)?;
        if token.is_empty() || token.len() > 16_384 || token.bytes().any(|b| b <= 32 || b == 127) {
            return Err(VaultError::Configuration);
        }
        let response = self.send(IpcMessage::ServiceCall {
            op: VaultOp::ServiceGetSecret {
                client_id: self.client_id.clone(),
                entry_id: binding.entry_id,
                field: "password".into(),
                token: token.into(),
            },
            stepup_approval: None,
        })?;
        Self::exact_secret_from_report(response)
    }

    fn send(&self, message: IpcMessage) -> Result<IpcMessage, VaultError> {
        let deadline = Instant::now() + self.timeout;
        let (reply, receive) = mpsc::sync_channel(1);
        self.sender
            .try_send(Work {
                message,
                deadline,
                reply,
            })
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => VaultError::Busy,
                mpsc::TrySendError::Disconnected(_) => VaultError::Configuration,
            })?;
        receive
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|e| match e {
                mpsc::RecvTimeoutError::Timeout => VaultError::Timeout,
                mpsc::RecvTimeoutError::Disconnected => VaultError::Configuration,
            })?
    }
}

/// An explicit daemon token file is authoritative; an invalid file never tries the user's
/// default configuration directory. Useful for isolated broker deployments and test fixtures.
fn client_with_token_file(
    socket: std::path::PathBuf,
    token_file: Option<&std::path::Path>,
) -> Result<IpcClient, VaultError> {
    let Some(path) = token_file else {
        return IpcClient::new(socket).map_err(|_| VaultError::Configuration);
    };
    use std::io::Read;
    if !std::fs::metadata(path)
        .map_err(|_| VaultError::Configuration)?
        .is_file()
    {
        return Err(VaultError::Configuration);
    }
    let mut token = String::new();
    std::fs::File::open(path)
        .map_err(|_| VaultError::Configuration)?
        .take(4097)
        .read_to_string(&mut token)
        .map_err(|_| VaultError::Configuration)?;
    if token.len() > 4096 || token.trim().is_empty() {
        return Err(VaultError::Configuration);
    }
    Ok(IpcClient::new_with_token(socket, token.trim().to_owned()))
}

impl Vault for SentinelPassIpcVault {
    fn name(&self) -> &'static str {
        "sentinelpass-ipc"
    }
    fn capabilities(&self) -> VaultCapabilities {
        VaultCapabilities {
            read: true,
            write: true,
            delete: false,
            bounded_io: true,
        }
    }

    fn get_secret(&self, provider: &str, label: &str) -> Result<Option<String>, VaultError> {
        super::validate_reference(provider, label)?;
        let response = self.send(IpcMessage::GetExternalSecret {
            client_id: self.client_id.clone(),
            domain: Self::domain(provider, label),
            field: sentinelpass_protocol::ExternalSecretField::Password,
            purpose: Some("sandhi-vault".into()),
        })?;
        if let IpcMessage::GetExternalSecretResponse {
            error: Some(error), ..
        } = &response
        {
            if Self::is_strict_legacy_denial(error) {
                return Err(VaultError::StrictLegacyDenied);
            }
        }
        match response {
            IpcMessage::GetExternalSecretResponse {
                authorized: false, ..
            } => Err(VaultError::Denied),
            IpcMessage::GetExternalSecretResponse {
                locked: Some(true), ..
            } => Err(VaultError::Locked),
            IpcMessage::GetExternalSecretResponse { error: Some(_), .. } => {
                Err(VaultError::Backend("broker lookup failed".into()))
            }
            IpcMessage::GetExternalSecretResponse {
                value: Some(value), ..
            } => {
                self.record_legacy_lookup(provider, label)?;
                Ok(Some(value))
            }
            IpcMessage::GetExternalSecretResponse {
                authorized: true,
                locked: None | Some(false),
                error: None,
                value: None,
            } => Ok(None),
            _ => Err(VaultError::Backend(
                "unexpected broker lookup response".into(),
            )),
        }
    }

    fn set_secret(&self, provider: &str, label: &str, secret: &str) -> Result<(), VaultError> {
        super::validate_reference(provider, label)?;
        match self.send(IpcMessage::SaveSecret {
            client_id: self.client_id.clone(),
            domain: Self::domain(provider, label),
            value: secret.into(),
            purpose: Some("sandhi-vault".into()),
        })? {
            IpcMessage::SaveSecretResponse {
                locked: Some(true), ..
            } => Err(VaultError::Locked),
            IpcMessage::SaveSecretResponse {
                success: true,
                error: None,
                ..
            } => Ok(()),
            IpcMessage::SaveSecretResponse { .. } => Err(VaultError::Denied),
            _ => Err(VaultError::Backend(
                "unexpected broker save response; reconcile before retry".into(),
            )),
        }
    }

    fn delete_secret(&self, _provider: &str, _label: &str) -> Result<bool, VaultError> {
        Err(VaultError::NotSupported(
            "external broker deletion is unsupported; revoke grants separately".into(),
        ))
    }

    fn reference_plan(
        &self,
        provider: &str,
        label: &str,
    ) -> Result<VaultReferencePlan, VaultError> {
        super::validate_reference(provider, label)?;
        Ok(VaultReferencePlan {
            backend: self.name(),
            resolution: "legacy_domain",
            searched_references: vec![Self::domain(provider, label)],
        })
    }

    fn get_exact_secret(
        &self,
        binding: &SentinelPassExactBinding,
    ) -> Result<Option<String>, VaultError> {
        let token = std::env::var(&binding.token_env)
            .ok()
            .filter(|value| !value.is_empty())
            .ok_or(VaultError::Configuration)?;
        self.get_exact_secret_with_token(binding, &token)
    }

    fn exact_reference_plan(
        &self,
        binding: &SentinelPassExactBinding,
    ) -> Result<VaultReferencePlan, VaultError> {
        super::validate_exact_binding(binding.entry_id, &binding.token_env)?;
        Ok(VaultReferencePlan {
            backend: self.name(),
            resolution: "exact_entry",
            searched_references: vec![
                format!("entry:{}", binding.entry_id),
                format!("token_env:{}", binding.token_env),
            ],
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn client(path: std::path::PathBuf) -> IpcClient {
        IpcClient::new_with_token(path, "synthetic-daemon-token".into())
            .with_context(Some("synthetic-read-token".into()), Some(Origin::Cli))
    }

    fn secure_exchange(
        listener: UnixListener,
        token: &'static str,
        write: bool,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            listener.set_nonblocking(true).unwrap();
            runtime.block_on(async {
                use sentinelpass_protocol::connection::IpcConnection;
                use sentinelpass_protocol::transport::unix::UnixSocketConnection;
                let listener = tokio::net::UnixListener::from_std(listener).unwrap();
                tokio::time::timeout(Duration::from_secs(2), async {
                    let (stream, _) = listener.accept().await.unwrap();
                    let (mut connection, first) = IpcConnection::accept_server(
                        UnixSocketConnection::from_stream(stream).into(),
                        token,
                    )
                    .await
                    .unwrap();
                    assert!(matches!(connection, IpcConnection::Secured { .. }));
                    assert!(first.is_none());
                    let body = connection.recv_frame().await;
                    if token != "synthetic-daemon-token" {
                        // A mismatched daemon token must never deliver an application request.
                        assert!(body.is_err());
                        return;
                    }
                    let envelope: sentinelpass_protocol::IpcEnvelope =
                        serde_json::from_slice(&body.unwrap()).unwrap();
                    assert_eq!(
                        envelope.client_token.as_deref(),
                        Some("synthetic-read-token")
                    );
                    assert_eq!(envelope.origin, Some(Origin::Cli));
                    let response = if write {
                        assert!(matches!(envelope.message, IpcMessage::SaveSecret {
                            client_id, domain, value, ..
                        } if client_id == "sandhi" && domain == "sandhi:openai:default"
                            && value == "synthetic-key"));
                        IpcMessage::SaveSecretResponse {
                            success: true,
                            error: None,
                            locked: None,
                        }
                    } else {
                        assert!(matches!(envelope.message, IpcMessage::GetExternalSecret {
                            client_id, domain, ..
                        } if client_id == "sandhi" && domain == "sandhi:openai:default"));
                        IpcMessage::GetExternalSecretResponse {
                            value: Some("synthetic-key".into()),
                            authorized: true,
                            error: None,
                            locked: None,
                        }
                    };
                    connection
                        .send_frame(&serde_json::to_vec(&response).unwrap())
                        .await
                        .unwrap();
                })
                .await
                .unwrap();
            });
        })
    }

    fn exact_exchange(listener: UnixListener) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            listener.set_nonblocking(true).unwrap();
            runtime.block_on(async {
                use sentinelpass_protocol::connection::IpcConnection;
                use sentinelpass_protocol::transport::unix::UnixSocketConnection;
                let listener = tokio::net::UnixListener::from_std(listener).unwrap();
                tokio::time::timeout(Duration::from_secs(2), async {
                    let (stream, _) = listener.accept().await.unwrap();
                    let (mut connection, _) = IpcConnection::accept_server(
                        UnixSocketConnection::from_stream(stream).into(),
                        "synthetic-daemon-token",
                    )
                    .await
                    .unwrap();
                    let body = connection.recv_frame().await.unwrap();
                    let envelope: sentinelpass_protocol::IpcEnvelope =
                        serde_json::from_slice(&body).unwrap();
                    assert!(matches!(
                        envelope.message,
                        IpcMessage::ServiceCall {
                            op: VaultOp::ServiceGetSecret {
                                ref client_id,
                                entry_id: 42,
                                ref field,
                                ref token,
                            },
                            stepup_approval: None,
                        } if client_id == "sandhi" && field == "password"
                            && token == "synthetic-service-token"
                    ));
                    let response = IpcMessage::ServiceResult {
                        outcome: ServiceOutcome::Ok {
                            result: VaultOpResult::Report(serde_json::json!({
                                "status": "authorized",
                                "value": "synthetic-exact-key",
                                "error": null,
                            })),
                        },
                    };
                    connection
                        .send_frame(&serde_json::to_vec(&response).unwrap())
                        .await
                        .unwrap();
                })
                .await
                .unwrap();
            });
        })
    }

    fn strict_legacy_exchange(listener: UnixListener) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            listener.set_nonblocking(true).unwrap();
            runtime.block_on(async {
                use sentinelpass_protocol::connection::IpcConnection;
                use sentinelpass_protocol::transport::unix::UnixSocketConnection;
                let listener = tokio::net::UnixListener::from_std(listener).unwrap();
                tokio::time::timeout(Duration::from_secs(2), async {
                    let (stream, _) = listener.accept().await.unwrap();
                    let (mut connection, _) = IpcConnection::accept_server(
                        UnixSocketConnection::from_stream(stream).into(),
                        "synthetic-daemon-token",
                    )
                    .await
                    .unwrap();
                    let body = connection.recv_frame().await.unwrap();
                    let envelope: sentinelpass_protocol::IpcEnvelope =
                        serde_json::from_slice(&body).unwrap();
                    assert!(matches!(
                        envelope.message,
                        IpcMessage::GetExternalSecret { .. }
                    ));
                    let response = IpcMessage::GetExternalSecretResponse {
                        value: None,
                        authorized: false,
                        error: Some(
                            "strict profile: legacy external-secret grants are disabled on this \
                             daemon; use exact-entry service grants"
                                .into(),
                        ),
                        locked: None,
                    };
                    connection
                        .send_frame(&serde_json::to_vec(&response).unwrap())
                        .await
                        .unwrap();
                })
                .await
                .unwrap();
            });
        })
    }

    #[test]
    fn construction_lookup_and_drop_are_safe_inside_an_async_runtime() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for write in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            // Protocol 0.13 requires an owner-only socket directory.
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            let socket = dir.path().join("broker.sock");
            let server = secure_exchange(
                UnixListener::bind(&socket).unwrap(),
                "synthetic-daemon-token",
                write,
            );
            runtime.block_on(async {
                let vault = SentinelPassIpcVault::from_client(
                    "sandhi".into(),
                    client(socket),
                    Duration::from_secs(1),
                )
                .unwrap();
                if write {
                    vault
                        .set_secret("openai", "default", "synthetic-key")
                        .unwrap();
                } else {
                    assert_eq!(
                        vault.get_secret("openai", "default").unwrap().as_deref(),
                        Some("synthetic-key")
                    );
                }
                drop(vault);
            });
            server.join().unwrap();
        }
    }

    #[test]
    fn exact_entry_retrieval_uses_service_get_secret_and_not_legacy_domain() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.path().join("broker.sock");
        let server = exact_exchange(UnixListener::bind(&socket).unwrap());
        runtime.block_on(async {
            let vault = SentinelPassIpcVault::from_client(
                "sandhi".into(),
                client(socket),
                Duration::from_secs(1),
            )
            .unwrap();
            let binding = SentinelPassExactBinding::new(42, "SANDHI_TEST_SERVICE_TOKEN").unwrap();
            assert_eq!(
                vault
                    .get_exact_secret_with_token(&binding, "synthetic-service-token")
                    .unwrap()
                    .as_deref(),
                Some("synthetic-exact-key")
            );
            drop(vault);
        });
        server.join().unwrap();
    }

    #[test]
    fn strict_legacy_denial_maps_on_the_wire_and_is_not_counted_as_use() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.path().join("broker.sock");
        let server = strict_legacy_exchange(UnixListener::bind(&socket).unwrap());
        runtime.block_on(async {
            let vault = SentinelPassIpcVault::from_client(
                "sandhi".into(),
                client(socket),
                Duration::from_secs(1),
            )
            .unwrap();
            assert!(matches!(
                vault.get_secret("openai", "default"),
                Err(VaultError::StrictLegacyDenied)
            ));
            assert_eq!(
                *vault
                    .legacy_lookups
                    .lock()
                    .unwrap()
                    .get("openai:default")
                    .unwrap_or(&0),
                0
            );
            drop(vault);
        });
        server.join().unwrap();
    }

    #[test]
    fn exact_and_legacy_reference_plans_are_distinct_without_contacting_a_daemon() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let vault = SentinelPassIpcVault {
            client_id: "sandhi".into(),
            sender,
            timeout: Duration::from_millis(50),
            legacy_lookups: Mutex::new(HashMap::new()),
        };
        let legacy = vault.reference_plan("openai", "default").unwrap();
        assert_eq!(legacy.backend, "sentinelpass-ipc");
        assert_eq!(legacy.resolution, "legacy_domain");
        assert_eq!(legacy.searched_references, vec!["sandhi:openai:default"]);
        let binding = SentinelPassExactBinding::new(42, "SANDHI_TEST_SERVICE_TOKEN").unwrap();
        let exact = vault.exact_reference_plan(&binding).unwrap();
        assert_eq!(exact.resolution, "exact_entry");
        assert_eq!(
            exact.searched_references,
            vec!["entry:42", "token_env:SANDHI_TEST_SERVICE_TOKEN"]
        );
        drop(receiver);
    }

    #[test]
    fn wrong_daemon_token_fails_closed_without_plaintext_retry() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.path().join("broker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let pending = listener.try_clone().unwrap();
        pending.set_nonblocking(true).unwrap();
        let server = secure_exchange(listener, "different-synthetic-token", false);
        let vault = SentinelPassIpcVault::from_client(
            "sandhi".into(),
            client(socket),
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(
            matches!(vault.get_secret("openai", "default"), Err(VaultError::Backend(ref message)) if message == "broker transport or protocol failure")
        );
        server.join().unwrap();
        assert!(matches!(pending.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    }

    #[test]
    fn handshake_obeys_the_vault_deadline() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = dir.path().join("broker.sock");
        // Leave the connection queued without acknowledging the session hello.
        let _listener = UnixListener::bind(&socket).unwrap();
        let vault = SentinelPassIpcVault::from_client(
            "sandhi".into(),
            client(socket),
            Duration::from_millis(50),
        )
        .unwrap();
        assert!(matches!(
            vault.get_secret("openai", "default"),
            Err(VaultError::Timeout)
        ));
    }

    #[test]
    fn public_socket_directory_is_rejected_before_connect() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        let socket = dir.path().join("broker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let vault = SentinelPassIpcVault::from_client(
            "sandhi".into(),
            client(socket),
            Duration::from_secs(1),
        )
        .unwrap();
        assert!(matches!(
            vault.get_secret("openai", "default"),
            Err(VaultError::Backend(_))
        ));
        assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    }

    #[test]
    fn queue_is_bounded_and_disconnection_is_explicit() {
        let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        let vault = SentinelPassIpcVault {
            client_id: "sandhi".into(),
            sender,
            timeout: Duration::from_millis(50),
            legacy_lookups: Mutex::new(HashMap::new()),
        };
        for _ in 0..QUEUE_CAPACITY {
            let (reply, _) = mpsc::sync_channel(1);
            assert!(vault
                .sender
                .try_send(Work {
                    message: IpcMessage::CheckVault,
                    deadline: Instant::now(),
                    reply,
                })
                .is_ok());
        }
        assert!(matches!(
            vault.get_secret("openai", "default"),
            Err(VaultError::Busy)
        ));
        drop(receiver);
        assert!(matches!(
            vault.get_secret("openai", "default"),
            Err(VaultError::Configuration)
        ));
    }

    #[test]
    fn expired_queued_work_never_reaches_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("broker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let vault = SentinelPassIpcVault::from_client(
            "sandhi".into(),
            client(socket),
            Duration::from_millis(100),
        )
        .unwrap();
        let (reply, receive) = mpsc::sync_channel(1);
        vault
            .sender
            .try_send(Work {
                message: IpcMessage::CheckVault,
                deadline: Instant::now() - Duration::from_secs(1),
                reply,
            })
            .ok()
            .unwrap();
        assert!(matches!(
            receive.recv_timeout(Duration::from_secs(1)).unwrap(),
            Err(VaultError::Timeout)
        ));
        assert!(matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock));
    }

    #[test]
    fn configuration_and_unsupported_delete_fail_before_transport() {
        let directory = tempfile::tempdir().unwrap();
        let token_file = directory.path().join("daemon-token");
        for data in [Vec::new(), b"  \n".to_vec(), vec![b'x'; 4097], vec![0xff]] {
            std::fs::write(&token_file, data).unwrap();
            assert!(matches!(
                client_with_token_file("unused".into(), Some(&token_file)),
                Err(VaultError::Configuration)
            ));
        }
        for path in [
            directory.path().join("missing"),
            directory.path().to_owned(),
        ] {
            assert!(matches!(
                client_with_token_file("unused".into(), Some(&path)),
                Err(VaultError::Configuration)
            ));
        }
        assert!(matches!(
            SentinelPassIpcVault::from_client(
                "sandhi".into(),
                client("unused".into()),
                Duration::ZERO
            ),
            Err(VaultError::Configuration)
        ));
        let vault = SentinelPassIpcVault::from_client(
            "sandhi".into(),
            client("unused".into()),
            Duration::from_millis(100),
        )
        .unwrap();
        assert!(matches!(
            vault.delete_secret("openai", "default"),
            Err(VaultError::NotSupported(_))
        ));
        assert!(matches!(
            vault.get_secret("openai", "*"),
            Err(VaultError::InvalidReference)
        ));
    }

    #[test]
    fn strict_legacy_denial_requires_the_complete_pinned_diagnostic() {
        let diagnostic = "strict profile: legacy external-secret grants are disabled on this \
                          daemon; use exact-entry service grants";
        assert!(SentinelPassIpcVault::is_strict_legacy_denial(diagnostic));
        for partial in [
            "strict profile",
            "legacy external-secret grants are disabled",
            "exact-entry service grants",
            "strict profile: legacy external-secret grants are disabled",
        ] {
            assert!(!SentinelPassIpcVault::is_strict_legacy_denial(partial));
        }
    }

    #[test]
    fn exact_entry_reports_map_typed_outcomes_without_relaying_errors() {
        let report = |status: &str, value: Option<&str>| IpcMessage::ServiceResult {
            outcome: ServiceOutcome::Ok {
                result: VaultOpResult::Report(serde_json::json!({
                    "status": status,
                    "value": value,
                    "error": null,
                })),
            },
        };
        assert_eq!(
            SentinelPassIpcVault::exact_secret_from_report(report(
                "authorized",
                Some("synthetic-exact-key")
            ))
            .unwrap()
            .as_deref(),
            Some("synthetic-exact-key")
        );
        assert_eq!(
            SentinelPassIpcVault::exact_secret_from_report(report("not_found", None)).unwrap(),
            None
        );
        assert!(matches!(
            SentinelPassIpcVault::exact_secret_from_report(report("locked", None)),
            Err(VaultError::Locked)
        ));
        assert!(matches!(
            SentinelPassIpcVault::exact_secret_from_report(report("denied", None)),
            Err(VaultError::Denied)
        ));
        assert!(matches!(
            SentinelPassIpcVault::exact_secret_from_report(report("authorized", None)),
            Err(VaultError::Backend(_))
        ));
        let service_error = IpcMessage::ServiceResult {
            outcome: ServiceOutcome::Err {
                error: sentinelpass_protocol::ServiceError::new("denied", "secret detail"),
            },
        };
        assert!(matches!(
            SentinelPassIpcVault::exact_secret_from_report(service_error),
            Err(VaultError::Denied)
        ));
    }
}
