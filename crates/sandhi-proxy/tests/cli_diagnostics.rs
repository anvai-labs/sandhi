//! Exercise the actual CLI binary, including exit status and error-output privacy.
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Output};
use std::time::Duration;

use sandhi_store::diagnostics::MAX_RESPONSE_BYTES;

const SELECTOR: &str = "SELECTOR_CANARY";
const TOKEN: &str = "CREDENTIAL_CANARY";
const BODY: &str = "RAW_BODY_CANARY";

fn cli() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_sandhi"));
    command
        .env_remove("SANDHI_ADMIN_URL")
        .env_remove("SANDHI_ADMIN_TOKEN")
        .env_remove("HTTP_PROXY")
        .env_remove("HTTPS_PROXY")
        .env_remove("ALL_PROXY")
        .env("NO_PROXY", "*");
    command
}

fn assert_private_failure(output: &Output) {
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.starts_with("error:"), "{stderr}");
    for canary in [
        SELECTOR,
        TOKEN,
        BODY,
        "URL_CANARY",
        "127.0.0.1",
        "Location:",
    ] {
        assert!(!stderr.contains(canary), "diagnostic error leaked {canary}");
    }
}

/// A localhost HTTP peer, not a mock of the reader: the test covers reqwest's framing too.
fn exchange(status: u16, body: Vec<u8>, chunked: bool, extra: &str) -> (Output, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let extra = extra.to_owned();
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut reader = BufReader::new(socket.try_clone().unwrap());
        let mut request = String::new();
        let mut length = 0;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line.to_ascii_lowercase().starts_with("content-length:") {
                length = line
                    .split_once(':')
                    .unwrap()
                    .1
                    .trim()
                    .parse::<usize>()
                    .unwrap();
            }
            request.push_str(&line);
            if line == "\r\n" {
                break;
            }
        }
        let mut request_body = vec![0; length];
        reader.read_exact(&mut request_body).unwrap();
        request.push_str(std::str::from_utf8(&request_body).unwrap());
        let framing = if chunked {
            "Transfer-Encoding: chunked\r\n".to_owned()
        } else {
            format!("Content-Length: {}\r\n", body.len())
        };
        let header = format!("HTTP/1.1 {status} Test\r\n{framing}{extra}Connection: close\r\n\r\n");
        socket.write_all(header.as_bytes()).unwrap();
        // Early rejection intentionally closes without consuming a non-2xx/oversized body.
        if chunked {
            let _ = write!(socket, "{:x}\r\n", body.len());
            let _ = socket.write_all(&body);
            let _ = socket.write_all(b"\r\n0\r\n\r\n");
        } else {
            let _ = socket.write_all(&body);
        }
        request
    });
    let output = cli()
        .args([
            "--admin-url",
            &format!("http://{address}"),
            "--admin-token",
            TOKEN,
            "diagnose",
            "--request",
            SELECTOR,
        ])
        .output()
        .unwrap();
    (output, server.join().unwrap())
}

#[test]
fn diagnostic_cli_posts_only_tagged_selector_and_prints_json_on_success() {
    let (output, request) = exchange(200, br#"{"rows":[],"returned_rows":0}"#.to_vec(), false, "");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap(),
        serde_json::json!({"rows":[],"returned_rows":0})
    );
    assert_eq!(
        request.lines().next().unwrap(),
        "POST /admin/usage/diagnostics HTTP/1.1"
    );
    assert!(request
        .to_ascii_lowercase()
        .contains("authorization: bearer credential_canary"));
    let body = request.split_once("\r\n\r\n").unwrap().1;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(body).unwrap(),
        serde_json::json!({"selector":{"kind":"request","value":SELECTOR},"limit":100})
    );
}

#[test]
fn diagnostic_cli_all_error_statuses_fail_without_reading_or_printing_bodies() {
    for status in [301, 307, 400, 401, 403, 404, 413, 429, 500, 503] {
        // A JSON object with no error key must still fail on a non-success status.
        let body = format!("{{\"detail\":\"{BODY}\"}}").into_bytes();
        let (output, _) = exchange(
            status,
            body,
            false,
            "Location: http://URL_CANARY.invalid/SELECTOR_CANARY\r\n",
        );
        assert_private_failure(&output);
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            "error: diagnostic request rejected\n"
        );
    }
}

#[test]
fn diagnostic_cli_bad_json_success_error_envelopes_and_empty_success_fail_private() {
    for body in [
        BODY.to_owned(),
        format!("{{\"error\":\"{BODY}\"}}"),
        String::new(),
    ] {
        let (output, _) = exchange(200, body.into_bytes(), false, "");
        assert_private_failure(&output);
    }
    let (output, _) = exchange(204, Vec::new(), false, "");
    assert_private_failure(&output);
}

#[test]
fn diagnostic_cli_caps_content_length_and_chunked_responses_before_json_parse() {
    for chunked in [false, true] {
        let mut body = b"{}".to_vec();
        body.resize(MAX_RESPONSE_BYTES + 1, b' ');
        let (output, _) = exchange(200, body, chunked, "");
        assert_private_failure(&output);
        assert_eq!(
            String::from_utf8_lossy(&output.stderr),
            "error: diagnostic response exceeds byte limit\n"
        );
    }
}

#[test]
fn diagnostic_cli_argument_transport_and_validation_errors_are_private_and_nonzero() {
    for args in [
        vec!["--request", SELECTOR, "--session", TOKEN],
        vec!["--request", SELECTOR, "--limit", TOKEN],
        vec!["--request", SELECTOR, "--unknown", TOKEN],
        vec!["--request", "\nSELECTOR_CANARY"],
        vec!["--request", SELECTOR, "--limit", "501"],
        vec!["--request", SELECTOR], // Invalid destination URL must not appear in errors.
        vec![],
    ] {
        let output = cli()
            .args([
                "--admin-url",
                "URL_CANARY",
                "--admin-token",
                TOKEN,
                "diagnose",
            ])
            .args(args)
            .output()
            .unwrap();
        assert_private_failure(&output);
    }
    let output = cli()
        .args(["diagnose", "--request", SELECTOR])
        .output()
        .unwrap();
    assert_private_failure(&output);
}
