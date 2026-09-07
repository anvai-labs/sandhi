//! Bounded synchronous facade over a thread-owned async broker client (TD-0026 W04).
//! No runtime is created, entered or dropped on the caller's async executor. Sync callers
//! still block waiting for a reply; async hosts must offload the entire operation.

use super::{Vault, VaultCapabilities, VaultError};
use sentinelpass_protocol::{IpcClient, IpcMessage, Origin};
use std::sync::mpsc::{self, SyncSender};
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
        let client = IpcClient::new(socket)
            .map_err(|_| VaultError::Configuration)?
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
        })
    }

    fn domain(provider: &str, label: &str) -> String {
        format!("sandhi:{provider}:{label}")
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
        match self.send(IpcMessage::GetExternalSecret {
            client_id: self.client_id.clone(),
            domain: Self::domain(provider, label),
            field: sentinelpass_protocol::ExternalSecretField::Password,
            purpose: Some("sandhi-vault".into()),
        })? {
            IpcMessage::GetExternalSecretResponse {
                authorized: false, ..
            } => Err(VaultError::Denied),
            IpcMessage::GetExternalSecretResponse {
                locked: Some(true), ..
            } => Err(VaultError::Locked),
            IpcMessage::GetExternalSecretResponse { error: Some(_), .. } => {
                Err(VaultError::Backend("broker lookup failed".into()))
            }
            IpcMessage::GetExternalSecretResponse { value, .. } => Ok(value),
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
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;

    fn client(path: std::path::PathBuf) -> IpcClient {
        IpcClient::new_with_token(path, "synthetic-daemon-token".into())
            .with_context(Some("synthetic-read-token".into()), Some(Origin::Cli))
    }

    #[test]
    fn construction_lookup_and_drop_are_safe_inside_an_async_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("broker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut prefix = [0; 4];
            stream.read_exact(&mut prefix).unwrap();
            let mut body = vec![0; u32::from_be_bytes(prefix) as usize];
            stream.read_exact(&mut body).unwrap();
            let envelope: sentinelpass_protocol::IpcEnvelope =
                serde_json::from_slice(&body).unwrap();
            assert_eq!(
                envelope.client_token.as_deref(),
                Some("synthetic-read-token")
            );
            assert!(
                matches!(envelope.message, IpcMessage::GetExternalSecret { domain, .. }
                if domain == "sandhi:openai:default")
            );
            let response = serde_json::to_vec(&IpcMessage::GetExternalSecretResponse {
                value: Some("synthetic-key".into()),
                authorized: true,
                error: None,
                locked: None,
            })
            .unwrap();
            stream
                .write_all(&(response.len() as u32).to_be_bytes())
                .unwrap();
            stream.write_all(&response).unwrap();
        });
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let vault = SentinelPassIpcVault::from_client(
                "sandhi".into(),
                client(socket),
                Duration::from_secs(1),
            )
            .unwrap();
            assert_eq!(
                vault.get_secret("openai", "default").unwrap().as_deref(),
                Some("synthetic-key")
            );
            drop(vault);
        });
        server.join().unwrap();
    }

    #[test]
    fn queue_is_bounded_and_disconnection_is_explicit() {
        let (sender, receiver) = mpsc::sync_channel(QUEUE_CAPACITY);
        let vault = SentinelPassIpcVault {
            client_id: "sandhi".into(),
            sender,
            timeout: Duration::from_millis(50),
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
}
