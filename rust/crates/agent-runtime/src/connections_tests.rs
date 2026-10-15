use super::*;
use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::mpsc,
    thread,
    time::Instant,
};

fn server(response: String) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/models", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            if socket.read(&mut byte).unwrap_or(0) == 0 {
                break;
            }
            bytes.push(byte[0]);
        }
        let _ = tx.send(String::from_utf8(bytes).unwrap());
        let _ = socket.write_all(response.as_bytes());
    });
    (url, rx, worker)
}
#[test]
fn model_transport_sends_auth_only_when_requested_and_parses_json() {
    for key in [Some("fixture-only"), None] {
        let body = r#"{"data":[{"id":"new-model"}]}"#;
        let (url, rx, worker) = server(format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ));
        let result = fetch_json(&url, key, &CancellationToken::new()).unwrap();
        assert_eq!(result["data"][0]["id"], "new-model");
        let request = rx.recv().unwrap().to_lowercase();
        assert_eq!(
            request.contains("authorization: bearer fixture-only"),
            key.is_some()
        );
        worker.join().unwrap();
    }
}
#[test]
fn transport_errors_and_redirects_never_echo_response_or_credentials() {
    for status in [401, 403, 429, 302, 500] {
        let body = "SECRET-REFLECTION";
        let (url, _, worker) = server(format!("HTTP/1.1 {status} Test\r\nLocation: https://example.com/leak\r\nContent-Length: {}\r\n\r\n{body}", body.len()));
        let error =
            fetch_json(&url, Some("fixture-secret"), &CancellationToken::new()).unwrap_err();
        assert_eq!(error, ConnectionError::Http(status));
        assert!(!error.to_string().contains("SECRET"));
        assert!(!error.to_string().contains("fixture-secret"));
        worker.join().unwrap();
    }
}
#[test]
fn oversized_or_invalid_response_is_rejected() {
    for (response, expected) in [
        (
            "HTTP/1.1 200 OK\r\nContent-Length: 99999999\r\n\r\n",
            ConnectionError::TooLarge,
        ),
        (
            "HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nbad",
            ConnectionError::InvalidModels,
        ),
    ] {
        let (url, _, worker) = server(response.into());
        assert_eq!(
            fetch_json(&url, None, &CancellationToken::new()).unwrap_err(),
            expected
        );
        worker.join().unwrap();
    }
}
#[test]
fn cancellation_interrupts_a_hanging_model_request() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let signal = CancellationToken::new();
    let cancel = signal.clone();
    let worker = thread::spawn(move || {
        let (_socket, _) = listener.accept().unwrap();
        cancel.cancel();
        thread::sleep(Duration::from_millis(200));
    });
    let start = Instant::now();
    assert_eq!(
        fetch_json(&url, None, &signal),
        Err(ConnectionError::Cancelled)
    );
    assert!(start.elapsed() < Duration::from_secs(2));
    worker.join().unwrap();
}
