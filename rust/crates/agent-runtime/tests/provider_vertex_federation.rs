use agent_core::types::{timestamp, CancellationToken};
use agent_runtime::provider::VertexOptions;
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

type Request = (String, BTreeMap<String, String>, String);
fn read_request(stream: &mut TcpStream) -> Request {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut byte = [0];
    while !bytes.ends_with(b"\r\n\r\n") {
        assert!(bytes.len() < 32768);
        stream.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
    }
    let text = String::from_utf8(bytes).unwrap();
    let mut lines = text.lines();
    let first = lines.next().unwrap().into();
    let headers: BTreeMap<String, String> = lines
        .filter_map(|line| {
            line.split_once(':')
                .map(|(key, value)| (key.to_lowercase(), value.trim().into()))
        })
        .collect();
    let size = headers
        .get("content-length")
        .map(|value| value.parse::<usize>().unwrap())
        .unwrap_or(0);
    let mut bytes = vec![0; size];
    stream.read_exact(&mut bytes).unwrap();
    (first, headers, String::from_utf8(bytes).unwrap())
}
fn server(responses: Vec<(u16, String)>) -> (String, std::thread::JoinHandle<Vec<Request>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let worker = std::thread::spawn(move || {
        let mut requests = vec![];
        for (status, body) in responses {
            let started = Instant::now();
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(started.elapsed() < Duration::from_secs(5));
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            requests.push(read_request(&mut stream));
            write!(stream,"HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
        }
        requests
    });
    (base, worker)
}
fn temp() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../target/vertex-federation-fixtures")
        .join(format!(
            "{}-{}-{}",
            std::process::id(),
            timestamp(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
    fs::create_dir_all(&path).unwrap();
    path
}
fn options(value: &Value) -> VertexOptions {
    let directory = temp();
    let path = directory.join("adc.json");
    fs::write(&path, value.to_string()).unwrap();
    VertexOptions {
        project: "fixture-project".into(),
        location: "us-central1".into(),
        adc_path: Some(path),
        ..Default::default()
    }
}
fn fields(body: &str) -> BTreeMap<String, String> {
    reqwest::Url::parse(&format!("http://fixture.invalid/?{body}"))
        .unwrap()
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}
fn external(token_url: &str, source: Value) -> Value {
    json!({"type":"external_account","audience":"//iam.googleapis.com/projects/123/locations/global/workloadIdentityPools/pool/providers/provider","subject_token_type":"urn:ietf:params:oauth:token-type:jwt","token_url":token_url,"credential_source":source,"quota_project_id":"quota-fixture"})
}
#[test]
fn file_oidc_and_saml_sources_send_actual_sts_exchange_and_cache() {
    for token_type in [
        "urn:ietf:params:oauth:token-type:jwt",
        "urn:ietf:params:oauth:token-type:saml2",
    ] {
        let directory = temp();
        let subject = directory.join("subject.txt");
        fs::write(&subject, "fixture-subject\n").unwrap();
        let (base, worker) = server(vec![(
            200,
            json!({"access_token":"fixture-google-token","token_type":"Bearer","expires_in":3600})
                .to_string(),
        )]);
        let mut value = external(&format!("{base}/sts"), json!({"file":subject}));
        value["subject_token_type"] = json!(token_type);
        let options = options(&value);
        let auth = options.authorize("", &CancellationToken::new()).unwrap();
        assert_eq!(auth.access_token.as_deref(), Some("fixture-google-token"));
        assert_eq!(auth.quota_project.as_deref(), Some("quota-fixture"));
        assert_eq!(
            options
                .authorize("", &CancellationToken::new())
                .unwrap()
                .access_token,
            auth.access_token
        );
        let requests = worker.join().unwrap();
        assert_eq!(requests.len(), 1);
        let body = fields(&requests[0].2);
        assert_eq!(
            body["grant_type"],
            "urn:ietf:params:oauth:grant-type:token-exchange"
        );
        assert_eq!(body["subject_token"], "fixture-subject\n");
        assert_eq!(body["subject_token_type"], token_type);
        assert_eq!(
            body["requested_token_type"],
            "urn:ietf:params:oauth:token-type:access_token"
        );
    }
}
#[test]
fn url_json_source_preserves_query_and_headers_then_exchanges_without_subject_leak() {
    let (base, worker) = server(vec![
        (200, json!({"id_token":"fixture-id-token"}).to_string()),
        (
            200,
            json!({"access_token":"fixture-google-token","expires_in":3600}).to_string(),
        ),
    ]);
    let value = external(
        &format!("{base}/sts"),
        json!({"url":format!("{base}/identity?resource=fixture"),"headers":{"Metadata":"true"},"format":{"type":"json","subject_token_field_name":"id_token"}}),
    );
    let auth = options(&value)
        .authorize("", &CancellationToken::new())
        .unwrap();
    assert_eq!(auth.access_token.as_deref(), Some("fixture-google-token"));
    let requests = worker.join().unwrap();
    assert!(requests[0].0.starts_with("GET /identity?resource=fixture"));
    assert_eq!(requests[0].1["metadata"], "true");
    assert_eq!(fields(&requests[1].2)["subject_token"], "fixture-id-token");
}
#[test]
fn workforce_user_project_and_basic_client_have_distinct_sts_options() {
    for basic in [false, true] {
        let directory = temp();
        let subject = directory.join("subject.json");
        fs::write(
            &subject,
            json!({"assertion":"fixture-assertion"}).to_string(),
        )
        .unwrap();
        let (base, worker) = server(vec![(
            200,
            json!({"access_token":"fixture-google-token","expires_in":3600}).to_string(),
        )]);
        let mut value = external(
            &format!("{base}/sts"),
            json!({"file":subject,"format":{"type":"json","subject_token_field_name":"assertion"}}),
        );
        value["audience"] =
            json!("//iam.googleapis.com/locations/global/workforcePools/pool/providers/provider");
        value["workforce_pool_user_project"] = json!("fixture-user-project");
        if basic {
            value["client_id"] = json!("fixture-client");
            value["client_secret"] = json!("fixture-secret");
        }
        options(&value)
            .authorize("", &CancellationToken::new())
            .unwrap();
        let requests = worker.join().unwrap();
        let body = fields(&requests[0].2);
        if basic {
            assert_eq!(
                requests[0].1["authorization"],
                format!("Basic {}", STANDARD.encode("fixture-client:fixture-secret"))
            );
            assert!(!body.contains_key("options"));
        } else {
            assert_eq!(
                serde_json::from_str::<Value>(&body["options"]).unwrap(),
                json!({"userProject":"fixture-user-project"})
            );
        }
    }
}
#[test]
fn external_account_can_impersonate_with_real_lifetime_scopes_and_expiry() {
    let directory = temp();
    let subject = directory.join("subject.txt");
    fs::write(&subject, "fixture-subject").unwrap();
    let (base, worker) = server(vec![
        (
            200,
            json!({"access_token":"fixture-sts-token","expires_in":3600}).to_string(),
        ),
        (
            200,
            json!({"accessToken":"fixture-impersonated","expireTime":"2999-01-01T00:00:00.125Z"})
                .to_string(),
        ),
    ]);
    let mut value = external(&format!("{base}/sts"), json!({"file":subject}));
    value["service_account_impersonation_url"]=json!(format!("{base}/v1/projects/-/serviceAccounts/fixture@project.iam.gserviceaccount.com:generateAccessToken"));
    value["service_account_impersonation"] = json!({"token_lifetime_seconds":7200});
    let auth = options(&value)
        .authorize("", &CancellationToken::new())
        .unwrap();
    assert_eq!(auth.access_token.as_deref(), Some("fixture-impersonated"));
    let requests = worker.join().unwrap();
    assert_eq!(requests[1].1["authorization"], "Bearer fixture-sts-token");
    let body: Value = serde_json::from_str(&requests[1].2).unwrap();
    assert_eq!(
        body,
        json!({"scope":["https://www.googleapis.com/auth/cloud-platform"],"lifetime":"7200s"})
    );
}
#[test]
fn impersonated_service_account_source_refresh_and_delegate_chain_are_native() {
    let (base, worker) = server(vec![
        (
            200,
            json!({"access_token":"fixture-source-user","expires_in":3600}).to_string(),
        ),
        (
            200,
            json!({"accessToken":"fixture-impersonated","expireTime":"2999-01-01T00:00:00Z"})
                .to_string(),
        ),
    ]);
    let value = json!({"type":"impersonated_service_account","source_credentials":{"type":"authorized_user","client_id":"fixture-client","client_secret":"fixture-secret","refresh_token":"fixture-refresh"},"service_account_impersonation_url":format!("{base}/v1/projects/-/serviceAccounts/fixture@project.iam.gserviceaccount.com:generateAccessToken"),"delegates":["projects/-/serviceAccounts/delegate@fixture.iam.gserviceaccount.com"]});
    let mut options = options(&value);
    options.token_url = format!("{base}/user-token");
    let auth = options.authorize("", &CancellationToken::new()).unwrap();
    assert_eq!(auth.access_token.as_deref(), Some("fixture-impersonated"));
    let requests = worker.join().unwrap();
    assert_eq!(fields(&requests[0].2)["grant_type"], "refresh_token");
    let body: Value = serde_json::from_str(&requests[1].2).unwrap();
    assert_eq!(
        body["delegates"],
        json!(["projects/-/serviceAccounts/delegate@fixture.iam.gserviceaccount.com"])
    );
    assert_eq!(body["lifetime"], "3600s");
}
#[test]
fn workforce_authorized_user_refresh_rotation_is_kept_in_memory_without_rewriting_adc() {
    let(base,worker)=server(vec![(200,json!({"access_token":"fixture-first","refresh_token":"fixture-rotated","expires_in":1}).to_string()),(200,json!({"access_token":"fixture-second","expires_in":3600}).to_string())]);
    let value = json!({"type":"external_account_authorized_user","client_id":"fixture-client","client_secret":"fixture-secret","refresh_token":"fixture-old","token_url":format!("{base}/workforce-token")});
    let options = options(&value);
    let path = options.adc_path.clone().unwrap();
    let before = fs::read(&path).unwrap();
    assert_eq!(
        options
            .authorize("", &CancellationToken::new())
            .unwrap()
            .access_token
            .as_deref(),
        Some("fixture-first")
    );
    assert_eq!(
        options
            .authorize("", &CancellationToken::new())
            .unwrap()
            .access_token
            .as_deref(),
        Some("fixture-second")
    );
    assert_eq!(fs::read(path).unwrap(), before);
    let requests = worker.join().unwrap();
    assert_eq!(fields(&requests[0].2)["refresh_token"], "fixture-old");
    assert_eq!(fields(&requests[1].2)["refresh_token"], "fixture-rotated");
    assert_eq!(
        requests[0].1["authorization"],
        format!("Basic {}", STANDARD.encode("fixture-client:fixture-secret"))
    );
}
#[test]
fn malformed_sources_excessive_nesting_and_real_http_errors_are_not_fake_auth_success() {
    let mut value = external(
        "http://127.0.0.1:1/sts",
        json!({"file":"fixture-absent","url":"http://127.0.0.1:1/identity"}),
    );
    assert!(options(&value)
        .authorize("", &CancellationToken::new())
        .is_err());
    value["credential_source"] = json!({"environment_id":"aws1"});
    assert!(options(&value)
        .authorize("", &CancellationToken::new())
        .err()
        .unwrap()
        .contains("not been migrated"));
    let (base, worker) = server(vec![(403, json!({"error":"fixture-secret"}).to_string())]);
    let value = json!({"type":"external_account_authorized_user","client_id":"fixture-client","client_secret":"fixture-secret","refresh_token":"fixture-refresh","token_url":format!("{base}/token")});
    let error = options(&value)
        .authorize("", &CancellationToken::new())
        .err()
        .unwrap();
    assert!(error.contains("HTTP 403"));
    assert!(!error.contains("fixture-secret"));
    worker.join().unwrap();
    let mut value = json!({"type":"authorized_user","client_id":"fixture-client","client_secret":"fixture-secret","refresh_token":"fixture-refresh"});
    for _ in 0..10 {
        value = json!({"type":"impersonated_service_account","source_credentials":value,"service_account_impersonation_url":"http://127.0.0.1:1/sa:generateAccessToken"});
    }
    assert!(options(&value)
        .authorize("", &CancellationToken::new())
        .err()
        .unwrap()
        .contains("nesting"));
}
#[test]
fn federation_header_stall_cancel_is_bounded_and_does_not_cache_a_token() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (sent, received) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        read_request(&mut stream);
        sent.send(()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let started = Instant::now();
        let mut byte = [0];
        loop {
            match stream.read(&mut byte) {
                Ok(0) => break,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    assert!(started.elapsed() < Duration::from_secs(3))
                }
                Err(_) => break,
                _ => {}
            }
        }
    });
    let value = json!({"type":"external_account_authorized_user","client_id":"fixture-client","client_secret":"fixture-secret","refresh_token":"fixture-refresh","token_url":format!("{base}/token")});
    let options = options(&value);
    let signal = CancellationToken::new();
    let copy = signal.clone();
    let caller = std::thread::spawn(move || options.authorize("", &copy));
    received.recv_timeout(Duration::from_secs(3)).unwrap();
    let start = Instant::now();
    signal.cancel();
    assert_eq!(caller.join().unwrap().err().unwrap(), "Operation aborted");
    assert!(start.elapsed() < Duration::from_secs(1));
    worker.join().unwrap();
}
