use super::*;
use std::{
    net::{SocketAddr, TcpListener},
    time::Duration,
};
fn free_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}
async fn wait_ready(client: &reqwest::Client, base: &str) {
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            if client
                .get(format!("{base}/readyz"))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("public serving path must initialize complete execution ownership");
}
#[tokio::test]
async fn public_serve_bootstraps_native_issuer_automatically_and_preserves_own_state_after_restart()
{
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("original.db");
    let path = path.to_str().unwrap();
    let origin = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(output(Family::OpenaiResponses, "A", false))
        .expect(3)
        .mount(&origin)
        .await;
    for restart in [false, true] {
        let state = uninitialized_state(
            path,
            provider(&origin.uri(), Family::OpenaiResponses, false),
            false,
        );
        let addr = free_addr();
        let base = format!("http://{addr}");
        let (tx, rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(sandhi_proxy::serve_with_shutdown_timeout(
            state.clone(),
            addr,
            async {
                let _ = rx.await;
            },
            Duration::from_secs(2),
        ));
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        wait_ready(&client, &base).await;
        if !restart {
            let response = client
                .post(format!("{base}/v1/responses"))
                .bearer_auth("tenant_A")
                .json(&body(Family::OpenaiResponses, "A", "fixture", false, false))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let response: Value = response.json().await.unwrap();
            assert_eq!(response["output"][0]["encrypted_content"], "opaque_A");
        }
        let response = client
            .post(format!("{base}/v1/responses"))
            .bearer_auth("tenant_A")
            .json(&body(Family::OpenaiResponses, "A", "fixture", false, true))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.json::<Value>().await.unwrap()["output"][0]["encrypted_content"],
            "opaque_A"
        );
        if !restart {
            let response = client
                .post(format!("{base}/v1/responses"))
                .bearer_auth("tenant_B")
                .json(&body(Family::OpenaiResponses, "A", "fixture", false, true))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
            assert_eq!(origin.received_requests().await.unwrap().len(), 2);
        }
        drop(client);
        let _ = tx.send(());
        server.await.unwrap().unwrap();
    }
    assert_eq!(origin.received_requests().await.unwrap().len(), 3);
    let conn = rusqlite::Connection::open(path).unwrap();
    let counts:(i64,i64,i64)=conn.query_row("SELECT (SELECT count(*) FROM budget_settlement_outbox),(SELECT count(*) FROM budget_reservation WHERE settled=1 AND actual=7),(SELECT count(*) FROM budget_reservation WHERE settled=0)",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
    assert_eq!(counts, (3, 6, 0));
}
#[tokio::test]
async fn malformed_registry_refuses_public_serve_before_bind_and_keeps_readonly_probes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("original.db");
    let path = path.to_str().unwrap();
    let state = uninitialized_state(
        path,
        provider("http://127.0.0.1:1", Family::OpenaiResponses, false),
        false,
    );
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute("CREATE TABLE issued_native_references(broken TEXT)", [])
        .unwrap();
    let held = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = held.local_addr().unwrap();
    let error = sandhi_proxy::serve_with_shutdown_timeout(
        state.clone(),
        addr,
        std::future::pending(),
        Duration::from_secs(1),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("native reference"), "{error}");
    assert_ne!(error.kind(), std::io::ErrorKind::AddrInUse);
    assert!(sandhi_proxy::initialize_execution(&state).is_err());
    let app = build_app(state);
    for (path, status) in [
        ("/healthz", StatusCode::OK),
        ("/readyz", StatusCode::SERVICE_UNAVAILABLE),
    ] {
        assert_eq!(
            app.clone()
                .oneshot(Request::get(path).body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status(),
            status
        );
    }
    assert_eq!(
        conn.query_row("SELECT count(*) FROM budget_execution_intent", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}
#[cfg(unix)]
#[tokio::test]
async fn readonly_registry_refuses_bootstrap_without_publishing_a_partial_owner_and_can_retry_after_repair(
) {
    use std::os::unix::fs::PermissionsExt;
    struct Restore {
        directory: std::path::PathBuf,
        files: Vec<(std::path::PathBuf, std::fs::Permissions)>,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ =
                std::fs::set_permissions(&self.directory, std::fs::Permissions::from_mode(0o700));
            for (path, mode) in &self.files {
                let _ = std::fs::set_permissions(path, mode.clone());
            }
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("original.db");
    let path_str = path.to_str().unwrap();
    let first = uninitialized_state(
        path_str,
        provider("http://127.0.0.1:1", Family::OpenaiResponses, false),
        false,
    );
    sandhi_proxy::initialize_execution(&first).unwrap();
    drop(first);
    let state = uninitialized_state(
        path_str,
        provider("http://127.0.0.1:1", Family::OpenaiResponses, false),
        false,
    );
    let mut restore = Restore {
        directory: dir.path().into(),
        files: Vec::new(),
    };
    for path in [
        &path,
        std::path::Path::new(&format!("{path_str}-wal")),
        std::path::Path::new(&format!("{path_str}-shm")),
    ] {
        if path.exists() {
            restore
                .files
                .push((path.into(), path.metadata().unwrap().permissions()));
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o400)).unwrap();
        }
    }
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
    assert!(sandhi_proxy::initialize_execution(&state).is_err());
    let held = TcpListener::bind("127.0.0.1:0").unwrap();
    let error = sandhi_proxy::serve_with_shutdown_timeout(
        state.clone(),
        held.local_addr().unwrap(),
        std::future::pending(),
        Duration::from_secs(1),
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("native reference"));
    assert_ne!(error.kind(), std::io::ErrorKind::AddrInUse);
    let app = build_app(state.clone());
    assert_eq!(
        app.oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    drop(restore);
    sandhi_proxy::initialize_execution(&state).unwrap();
}
