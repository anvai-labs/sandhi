//! Test-only secured IPC bridge for the Python SDK broker fixture.
//! Authorization remains in FakeBroker; no vault or user credentials are loaded.
#[cfg(unix)]
mod fixture {
    use sentinelpass_protocol::connection::IpcConnection;
    use sentinelpass_protocol::transport::unix::UnixSocketConnection;
    use sentinelpass_protocol::{IpcEnvelope, IpcMessage};
    use std::io;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const FRAME_LIMIT: usize = 65_536;
    const CONTROL_TIMEOUT: Duration = Duration::from_secs(3);

    async fn write_control(output: &mut tokio::io::Stdout, body: &[u8]) -> io::Result<()> {
        if body.len() > FRAME_LIMIT {
            return Err(io::Error::other("fixture response too large"));
        }
        tokio::time::timeout(CONTROL_TIMEOUT, async {
            output.write_u32(body.len() as u32).await?;
            output.write_all(body).await?;
            output.flush().await
        })
        .await?
    }

    async fn read_control(input: &mut tokio::io::Stdin) -> io::Result<Vec<u8>> {
        tokio::time::timeout(CONTROL_TIMEOUT, async {
            let size = input.read_u32().await? as usize;
            if size == 0 || size > FRAME_LIMIT {
                return Err(io::Error::other("invalid fixture frame size"));
            }
            let mut body = vec![0; size];
            input.read_exact(&mut body).await?;
            Ok(body)
        })
        .await?
    }

    async fn run() -> Result<(), Box<dyn std::error::Error>> {
        let path = std::env::args().nth(1).ok_or("fixture socket required")?;
        let listener = tokio::net::UnixListener::bind(path)?;
        let mut input = tokio::io::stdin();
        let mut output = tokio::io::stdout();
        write_control(&mut output, br#"{"ready":true}"#).await?;
        loop {
            let (stream, _) = listener.accept().await?;
            let received = tokio::time::timeout(Duration::from_secs(2), async {
                let (mut connection, first) = IpcConnection::accept_server(
                    UnixSocketConnection::from_stream(stream).into(),
                    "disposable-daemon-token",
                )
                .await?;
                // Refuse plaintext even if the parent's environment enabled migration.
                if !matches!(connection, IpcConnection::Secured { .. }) || first.is_some() {
                    return Err(sentinelpass_protocol::transport::TransportError::Other(
                        "secured fixture session required".into(),
                    ));
                }
                let body = connection.recv_frame().await?;
                Ok((connection, body))
            })
            .await;
            let Ok(Ok((mut connection, body))) = received else {
                // A disconnected, unauthenticated or stalled client cannot wedge accept.
                continue;
            };
            let envelope: IpcEnvelope = serde_json::from_slice(&body)?;
            write_control(&mut output, &serde_json::to_vec(&envelope)?).await?;
            let body = read_control(&mut input).await?;
            // A null control response models the existing broker hang/connection loss.
            let response: Option<IpcMessage> = serde_json::from_slice(&body)?;
            if let Some(response) = response {
                // A proxy may time out before the synthetic reply. Drop that connection
                // and accept the next one; never associate its reply with another client.
                let body = serde_json::to_vec(&response)?;
                let _ = tokio::time::timeout(CONTROL_TIMEOUT, connection.send_frame(&body)).await;
            }
        }
    }

    pub fn main() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("fixture runtime");
        let result = runtime.block_on(run());
        // Tokio's stdio adapter uses blocking reads; a control timeout must not make
        // runtime destruction wait forever for an abandoned Python pipe.
        runtime.shutdown_timeout(Duration::from_secs(1));
        if result.is_err() {
            eprintln!("secured SDK fixture transport failed");
            std::process::exit(1);
        }
    }
}

#[cfg(unix)]
fn main() {
    fixture::main();
}

#[cfg(not(unix))]
fn main() {
    eprintln!("Unix SDK fixture only");
    std::process::exit(2);
}
