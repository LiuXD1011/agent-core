//! Isolated token caches and loopback HTTP only. SDK oracle uses a custom
//! request handler; none of these fixtures inspect ambient AWS configuration.
use super::*;
use std::net::{TcpListener, TcpStream};
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
use std::process::{Command, Stdio};
use std::sync::{mpsc, Barrier};
use std::thread;
#[cfg(windows)]
use windows_sys::Win32::Foundation::LocalFree;
#[cfg(windows)]
use windows_sys::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW,
};
#[cfg(windows)]
use windows_sys::Win32::Security::{SetFileSecurityW, PROTECTED_DACL_SECURITY_INFORMATION};
#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};

const START: &str = "https://fixture.awsapps.com/start";
const SESSION: &str = "native-fixture-session";
const SDK_ORACLE: &str = include_str!("../tests/fixtures/aws-sso.json");

struct Files(PathBuf);
impl Files {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("native-sso-fixture-{}", nonce().unwrap()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }
    fn plan(&self, modern: bool, endpoint: &str) -> SsoPlan {
        SsoPlan {
            start_url: START.into(),
            region: "us-west-2".into(),
            account_id: "123456789012".into(),
            role_name: "Developer+Read Only".into(),
            session_name: modern.then(|| SESSION.into()),
            cache_dir: self.0.clone(),
            endpoint: endpoint.into(),
            oidc_endpoint: endpoint.into(),
        }
    }
    fn write(&self, plan: &SsoPlan, value: &Value) -> Vec<u8> {
        let bytes = serde_json::to_vec_pretty(value).unwrap();
        std::fs::write(cache_path(plan), &bytes).unwrap();
        bytes
    }
}
impl Drop for Files {
    fn drop(&mut self) {
        for file in std::fs::read_dir(&self.0).unwrap() {
            std::fs::remove_file(file.unwrap().path()).unwrap();
        }
        std::fs::remove_dir(&self.0).unwrap();
    }
}
fn date(seconds: u64) -> String {
    let date = aws_timestamp(seconds).unwrap();
    format!(
        "{}-{}-{}T{}:{}:{}Z",
        &date[..4],
        &date[4..6],
        &date[6..8],
        &date[9..11],
        &date[11..13],
        &date[13..15]
    )
}
fn token(expires: u64) -> Value {
    json!({"accessToken":"old-access-fixture", "expiresAt":date(expires),
        "refreshToken":"old-refresh-fixture", "clientId":"client-id-fixture", "clientSecret":"client-secret-fixture",
        "registrationExpiresAt":"2030-01-01T00:00:00Z", "startUrl":START,"region":"us-west-2", "unknown":{"preserved":true}})
}
fn role() -> Value {
    json!({"roleCredentials":{"accessKeyId":"ASIAFIXTURE","secretAccessKey":"role-secret-fixture", "sessionToken":"role-session-fixture","expiration":1_893_456_000_123u64}})
}
fn renewed() -> Value {
    json!({"accessToken":"new-access-fixture","refreshToken":"new-refresh-fixture","expiresIn":3600,"tokenType":"Bearer"})
}
struct Request {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}
fn accept(listener: &TcpListener) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let started = Instant::now();
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                return stream;
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    && started.elapsed() < Duration::from_secs(3) =>
            {
                thread::sleep(Duration::from_millis(5))
            }
            Err(error) => panic!("Fixture accept: {error}"),
        }
    }
}
fn read_request(stream: &mut TcpStream) -> Request {
    let mut bytes = vec![];
    let mut chunk = [0u8; 4096];
    let (end, length) = loop {
        let count = stream.read(&mut chunk).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&chunk[..count]);
        assert!(bytes.len() < 100_000);
        if let Some(index) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            let length = String::from_utf8_lossy(&bytes[..index])
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            break (index + 4, length);
        }
    };
    while bytes.len() < end + length {
        let count = stream.read(&mut chunk).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&chunk[..count]);
    }
    let head = String::from_utf8_lossy(&bytes[..end]);
    let mut first = head.lines().next().unwrap().split_whitespace();
    Request {
        method: first.next().unwrap().into(),
        path: first.next().unwrap().into(),
        headers: head
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.to_lowercase(), value.trim().into()))
            .collect(),
        body: bytes[end..end + length].to_vec(),
    }
}
fn respond(stream: &mut TcpStream, status: u16, value: &Value) {
    let bytes = serde_json::to_vec(value).unwrap();
    write!(stream, "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", bytes.len()).unwrap();
    stream.write_all(&bytes).unwrap();
}
fn serve(responses: Vec<(u16, Value)>) -> (String, thread::JoinHandle<Vec<Request>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        responses
            .iter()
            .map(|(status, value)| {
                let mut stream = accept(&listener);
                let request = read_request(&mut stream);
                respond(&mut stream, *status, value);
                request
            })
            .collect()
    });
    (endpoint, server)
}
fn error(result: Result<CredentialResult, String>) -> String {
    result.err().expect("expected credential error")
}
fn no_secrets(error: &str) {
    for secret in [
        "old-access-fixture",
        "new-access-fixture",
        "old-refresh-fixture",
        "new-refresh-fixture",
        "client-secret-fixture",
        "role-secret-fixture",
        "role-session-fixture",
        "remote-secret-fixture",
    ] {
        assert!(!error.contains(secret), "error echoed fixture credential");
    }
}
fn verify_sdk(request: &Request, action: &str) {
    let oracle: Value = serde_json::from_str(SDK_ORACLE).unwrap();
    assert_eq!(oracle["version"], "3.997.9");
    let case = oracle["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["action"] == action)
        .unwrap();
    assert_eq!(request.method, case["method"].as_str().unwrap());
    let url = Url::parse(&format!("https://fixture.invalid{}", request.path)).unwrap();
    assert_eq!(url.path(), case["path"].as_str().unwrap());
    let query: BTreeMap<_, _> = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    assert_eq!(serde_json::to_value(query).unwrap(), case["query"]);
    assert_eq!(
        request
            .headers
            .get("x-amz-sso_bearer_token")
            .map(String::as_str),
        case["token_header"].as_str()
    );
    assert!(!request.headers.contains_key("authorization"));
    assert_eq!(
        request.headers.get("content-type").map(String::as_str),
        case["content_type"].as_str()
    );
    let body: Value = if request.body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&request.body).unwrap()
    };
    assert_eq!(body, case["body"]);
}

#[test]
fn sdk_sha1_cache_names_and_regional_endpoints() {
    let files = Files::new();
    let oracle: Value = serde_json::from_str(SDK_ORACLE).unwrap();
    for modern in [false, true] {
        let plan = files.plan(modern, "");
        let id = if modern { SESSION } else { START };
        assert_eq!(
            cache_path(&plan).file_name().unwrap().to_str().unwrap(),
            oracle["hashes"][id]
        );
        assert_eq!(
            endpoint(&plan, false).unwrap().as_str(),
            "https://portal.sso.us-west-2.amazonaws.com/federation/credentials"
        );
        assert_eq!(
            endpoint(&plan, true).unwrap().as_str(),
            "https://oidc.us-west-2.amazonaws.com/token"
        );
    }
    let mut plan = files.plan(true, "");
    plan.region = "cn-north-1".into();
    assert_eq!(
        endpoint(&plan, true).unwrap().as_str(),
        "https://oidc.cn-north-1.amazonaws.com.cn/token"
    );
}
#[test]
fn legacy_native_get_matches_sdk_and_millisecond_expiration() {
    let files = Files::new();
    let (url, server) = serve(vec![(200, role())]);
    let plan = files.plan(false, &url);
    let original = files.write(&plan, &token(timestamp() / 1000 + 3600));
    let result = resolve(&plan, &CancellationToken::new()).unwrap();
    assert_eq!(result.credentials.access_key_id, "ASIAFIXTURE");
    assert_eq!(result.expiration, Some(1_893_456_000));
    verify_sdk(&server.join().unwrap()[0], "GetRoleCredentials");
    assert_eq!(std::fs::read(cache_path(&plan)).unwrap(), original);
}
#[test]
fn modern_fresh_token_needs_no_refresh_or_cache_write() {
    let files = Files::new();
    let (url, server) = serve(vec![(200, role())]);
    let plan = files.plan(true, &url);
    let original = files.write(&plan, &token(timestamp() / 1000 + 3600));
    resolve(&plan, &CancellationToken::new()).unwrap();
    assert_eq!(server.join().unwrap()[0].method, "GET");
    assert_eq!(std::fs::read(cache_path(&plan)).unwrap(), original);
    assert!(!sidecar(&cache_path(&plan)).exists());
}
#[test]
fn expired_legacy_token_does_not_refresh_or_mutate_cache() {
    let files = Files::new();
    let plan = files.plan(false, "http://127.0.0.1:9");
    let original = files.write(&plan, &token(1));
    let result = error(resolve(&plan, &CancellationToken::new()));
    assert_eq!(result, LOGIN_REQUIRED);
    assert_eq!(std::fs::read(cache_path(&plan)).unwrap(), original);
}
#[test]
fn modern_refresh_matches_sdk_preserves_unknown_fields_and_changes_fingerprint() {
    let files = Files::new();
    let (url, server) = serve(vec![(200, renewed()), (200, role())]);
    let plan = files.plan(true, &url);
    let original = files.write(&plan, &token(1));
    let old_hash = fingerprint(&plan, &CancellationToken::new()).unwrap();
    resolve(&plan, &CancellationToken::new()).unwrap();
    let requests = server.join().unwrap();
    verify_sdk(&requests[0], "CreateToken");
    assert_eq!(
        requests[1].headers["x-amz-sso_bearer_token"],
        "new-access-fixture"
    );
    let value: Value = serde_json::from_slice(&std::fs::read(cache_path(&plan)).unwrap()).unwrap();
    assert_eq!(value["unknown"], json!({"preserved":true}));
    assert_eq!(value["refreshToken"], "new-refresh-fixture");
    assert_eq!(value["clientSecret"], "client-secret-fixture");
    assert!(
        parse_expiration(value["expiresAt"].as_str().unwrap()).unwrap() > timestamp() / 1000 + 3000
    );
    assert_ne!(
        fingerprint(&plan, &CancellationToken::new()).unwrap(),
        old_hash
    );
    assert_ne!(std::fs::read(cache_path(&plan)).unwrap(), original);
    assert_eq!(std::fs::read_dir(&files.0).unwrap().count(), 1);
}
#[test]
fn absent_rotated_refresh_token_preserves_previous_value() {
    let files = Files::new();
    let (url, server) = serve(vec![
        (
            200,
            json!({"accessToken":"new-access-fixture","expiresIn":3600}),
        ),
        (200, role()),
    ]);
    let plan = files.plan(true, &url);
    files.write(&plan, &token(1));
    resolve(&plan, &CancellationToken::new()).unwrap();
    server.join().unwrap();
    let value: Value = serde_json::from_slice(&std::fs::read(cache_path(&plan)).unwrap()).unwrap();
    assert_eq!(value["refreshToken"], "old-refresh-fixture");
}
#[test]
fn role_failure_after_refresh_keeps_rotated_token_commit() {
    let files = Files::new();
    let (url, server) = serve(vec![
        (200, renewed()),
        (403, json!({"message":"remote-secret-fixture"})),
    ]);
    let plan = files.plan(true, &url);
    files.write(&plan, &token(1));
    let result = error(resolve(&plan, &CancellationToken::new()));
    assert!(result.contains("HTTP 403"));
    no_secrets(&result);
    server.join().unwrap();
    let value: Value = serde_json::from_slice(&std::fs::read(cache_path(&plan)).unwrap()).unwrap();
    assert_eq!(value["refreshToken"], "new-refresh-fixture");
    assert!(!sidecar(&cache_path(&plan)).exists());
}
#[test]
fn missing_or_expired_registration_and_refresh_failures_preserve_valid_old_token() {
    for variant in 0..4 {
        let files = Files::new();
        let responses = if variant == 3 {
            vec![
                (400, json!({"error":"remote-secret-fixture"})),
                (200, role()),
            ]
        } else {
            vec![(200, role())]
        };
        let (url, server) = serve(responses);
        let plan = files.plan(true, &url);
        let mut cached = token(timestamp() / 1000 + 120);
        match variant {
            0 => {
                cached
                    .as_object_mut()
                    .unwrap()
                    .remove("registrationExpiresAt");
            }
            1 => cached["registrationExpiresAt"] = json!(date(1)),
            2 => {
                cached.as_object_mut().unwrap().remove("clientSecret");
            }
            _ => {}
        }
        let original = files.write(&plan, &cached);
        resolve(&plan, &CancellationToken::new()).unwrap();
        let requests = server.join().unwrap();
        assert_eq!(
            requests.last().unwrap().headers["x-amz-sso_bearer_token"],
            "old-access-fixture"
        );
        assert_eq!(std::fs::read(cache_path(&plan)).unwrap(), original);
    }
}
#[test]
fn expired_modern_token_requires_valid_registration_no_fake_success() {
    for field in [
        "registrationExpiresAt",
        "clientId",
        "clientSecret",
        "refreshToken",
    ] {
        let files = Files::new();
        let plan = files.plan(true, "http://127.0.0.1:9");
        let mut cached = token(1);
        cached.as_object_mut().unwrap().remove(field);
        let original = files.write(&plan, &cached);
        assert_eq!(
            error(resolve(&plan, &CancellationToken::new())),
            LOGIN_REQUIRED
        );
        assert_eq!(std::fs::read(cache_path(&plan)).unwrap(), original);
        assert!(!sidecar(&cache_path(&plan)).exists());
    }
}
#[test]
fn failed_or_malformed_refresh_does_not_write_or_echo_credentials() {
    for (status, value) in [
        (400, json!({"error_description":"remote-secret-fixture"})),
        (
            200,
            json!({"accessToken":"remote-secret-fixture","expiresIn":0}),
        ),
        (
            200,
            json!({"accessToken":"remote-secret-fixture","expiresIn":3600,"tokenType":"wrong"}),
        ),
        (
            200,
            json!({"accessToken":"remote-secret-fixture","expiresIn":3600,"refreshToken":null}),
        ),
    ] {
        let files = Files::new();
        let (url, server) = serve(vec![(status, value)]);
        let plan = files.plan(true, &url);
        let original = files.write(&plan, &token(1));
        no_secrets(&error(resolve(&plan, &CancellationToken::new())));
        server.join().unwrap();
        assert_eq!(std::fs::read(cache_path(&plan)).unwrap(), original);
        assert_eq!(std::fs::read_dir(&files.0).unwrap().count(), 1);
    }
}
#[test]
fn cache_identity_and_schema_bounds_reject_before_http() {
    let files = Files::new();
    let plan = files.plan(false, "http://127.0.0.1:9");
    for bytes in [b"[]".to_vec(),b"{broken".to_vec(),vec![b' ';FILE_LIMIT+1],serde_json::to_vec(&json!({"accessToken":"fixture","expiresAt":"not-date"})).unwrap(),serde_json::to_vec(&json!({"accessToken":"fixture","expiresAt":"2030-01-01T00:00:00Z","startUrl":"https://other.invalid/start"})).unwrap(),serde_json::to_vec(&json!({"accessToken":"fixture","expiresAt":"2030-01-01T00:00:00Z","region":"us-east-1"})).unwrap()] {
        std::fs::write(cache_path(&plan),&bytes).unwrap();
        no_secrets(&error(resolve(&plan,&CancellationToken::new())));
        assert_eq!(std::fs::read(cache_path(&plan)).unwrap(),bytes);
    }
}
#[test]
fn modern_issuer_url_accepts_portal_start_url_in_session_cache() {
    let files = Files::new();
    let (url, server) = serve(vec![(200, role())]);
    let mut plan = files.plan(true, &url);
    plan.start_url = "https://identitycenter.us-west-2.amazonaws.com/ssoins-fixture".into();
    let original = files.write(&plan, &token(timestamp() / 1000 + 3600));
    resolve(&plan, &CancellationToken::new()).unwrap();
    verify_sdk(&server.join().unwrap()[0], "GetRoleCredentials");
    assert_eq!(std::fs::read(cache_path(&plan)).unwrap(), original);
}
fn wait_closed(stream: &mut TcpStream) {
    stream
        .set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let started = Instant::now();
    let mut byte = [0u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => return,
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
                ) =>
            {
                return
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) && started.elapsed() < Duration::from_secs(2) => {}
            Err(error) => panic!("fixture client did not close: {error}"),
        }
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
#[test]
fn precommit_cancel_aborts_header_and_body_stalls_without_writes() {
    for partial in [false, true] {
        let files = Files::new();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let plan = files.plan(true, &url);
        let original = files.write(&plan, &token(1));
        let (ready, received) = mpsc::channel();
        let server = thread::spawn(move || {
            let mut stream = accept(&listener);
            assert_eq!(read_request(&mut stream).method, "POST");
            if partial {
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3000\r\n\r\n{")
                    .unwrap();
            }
            ready.send(()).unwrap();
            wait_closed(&mut stream);
        });
        let signal = CancellationToken::new();
        let worker_signal = signal.clone();
        let worker_plan = plan.clone();
        let worker = thread::spawn(move || resolve(&worker_plan, &worker_signal));
        received.recv_timeout(Duration::from_secs(3)).unwrap();
        let started = Instant::now();
        signal.cancel();
        assert_eq!(error(worker.join().unwrap()), "Operation aborted");
        assert!(started.elapsed() < Duration::from_secs(2));
        server.join().unwrap();
        assert_eq!(std::fs::read(cache_path(&plan)).unwrap(), original);
        assert_eq!(std::fs::read_dir(&files.0).unwrap().count(), 1);
    }
}
#[test]
fn cancellation_does_not_fallback_to_still_valid_old_token() {
    let files = Files::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let plan = files.plan(true, &format!("http://{}", listener.local_addr().unwrap()));
    let original = files.write(&plan, &token(timestamp() / 1000 + 120));
    let signal = CancellationToken::new();
    let cancel = signal.clone();
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        assert_eq!(read_request(&mut stream).method, "POST");
        cancel.cancel();
        wait_closed(&mut stream);
    });
    assert_eq!(error(resolve(&plan, &signal)), "Operation aborted");
    server.join().unwrap();
    assert_eq!(std::fs::read(cache_path(&plan)).unwrap(), original);
}
#[test]
fn postcommit_role_cancellation_keeps_rotated_token() {
    let files = Files::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let plan = files.plan(true, &format!("http://{}", listener.local_addr().unwrap()));
    files.write(&plan, &token(1));
    let signal = CancellationToken::new();
    let cancel = signal.clone();
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        read_request(&mut stream);
        respond(&mut stream, 200, &renewed());
        drop(stream);
        let mut stream = accept(&listener);
        assert_eq!(
            read_request(&mut stream).headers["x-amz-sso_bearer_token"],
            "new-access-fixture"
        );
        cancel.cancel();
        wait_closed(&mut stream);
    });
    assert_eq!(error(resolve(&plan, &signal)), "Operation aborted");
    server.join().unwrap();
    let value: Value = serde_json::from_slice(&std::fs::read(cache_path(&plan)).unwrap()).unwrap();
    assert_eq!(value["refreshToken"], "new-refresh-fixture");
}
#[test]
fn concurrent_roles_share_one_token_rotation() {
    let files = Files::new();
    let (url, server) = serve(vec![(200, renewed()), (200, role()), (200, role())]);
    let plan = files.plan(true, &url);
    files.write(&plan, &token(1));
    let barrier = Arc::new(Barrier::new(2));
    let workers: Vec<_> = (0..2)
        .map(|index| {
            let mut plan = plan.clone();
            plan.role_name = format!("fixture-role-{index}");
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                resolve(&plan, &CancellationToken::new())
                    .unwrap()
                    .credentials
                    .access_key_id
            })
        })
        .collect();
    for worker in workers {
        assert_eq!(worker.join().unwrap(), "ASIAFIXTURE");
    }
    let requests = server.join().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "POST")
            .count(),
        1
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "GET")
            .count(),
        2
    );
}
#[test]
fn external_cache_replacement_is_not_lost_during_refresh() {
    let files = Files::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let plan = files.plan(true, &format!("http://{}", listener.local_addr().unwrap()));
    files.write(&plan, &token(1));
    let path = cache_path(&plan);
    let mut external = token(timestamp() / 1000 + 7200);
    external["accessToken"] = json!("external-login-fixture");
    let external = serde_json::to_vec(&external).unwrap();
    let expected = external.clone();
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        read_request(&mut stream);
        std::fs::write(path, external).unwrap();
        respond(&mut stream, 200, &renewed());
    });
    assert_eq!(
        error(resolve(&plan, &CancellationToken::new())),
        "AWS SSO token cache changed during refresh"
    );
    server.join().unwrap();
    assert_eq!(std::fs::read(cache_path(&plan)).unwrap(), expected);
    assert_eq!(std::fs::read_dir(&files.0).unwrap().count(), 1);
}
#[test]
fn role_credentials_fingerprint_binds_sent_token_not_later_external_login() {
    let files = Files::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let plan = files.plan(true, &format!("http://{}", listener.local_addr().unwrap()));
    let original = files.write(&plan, &token(timestamp() / 1000 + 3600));
    let original_hash = token_fingerprint(&plan, &original);
    let path = cache_path(&plan);
    let mut external = token(timestamp() / 1000 + 7200);
    external["accessToken"] = json!("external-login-fixture");
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        assert_eq!(
            read_request(&mut stream).headers["x-amz-sso_bearer_token"],
            "old-access-fixture"
        );
        std::fs::write(path, serde_json::to_vec(&external).unwrap()).unwrap();
        respond(&mut stream, 200, &role());
    });
    let result = resolve(&plan, &CancellationToken::new()).unwrap();
    server.join().unwrap();
    assert_eq!(
        result.source_fingerprint.as_deref(),
        Some(original_hash.as_str())
    );
    assert_ne!(
        fingerprint(&plan, &CancellationToken::new()).unwrap(),
        original_hash
    );
}
#[test]
fn external_lock_wait_is_cancellable_and_never_removes_foreign_lock() {
    let files = Files::new();
    let plan = files.plan(true, "http://127.0.0.1:9");
    let original = files.write(&plan, &token(1));
    std::fs::write(sidecar(&cache_path(&plan)), b"external-fixture-lock").unwrap();
    let signal = CancellationToken::new();
    let cancel = signal.clone();
    let canceller = thread::spawn(move || {
        thread::sleep(Duration::from_millis(75));
        cancel.cancel();
    });
    let started = Instant::now();
    assert_eq!(error(resolve(&plan, &signal)), "Operation aborted");
    assert!(started.elapsed() < Duration::from_secs(2));
    canceller.join().unwrap();
    assert_eq!(
        std::fs::read(sidecar(&cache_path(&plan))).unwrap(),
        b"external-fixture-lock"
    );
    assert_eq!(std::fs::read(cache_path(&plan)).unwrap(), original);
}
#[test]
fn malformed_expired_role_credentials_and_large_response_fail_without_secrets() {
    for value in [
        json!({}),
        json!({"roleCredentials":{"accessKeyId":"ASIAFIXTURE","secretAccessKey":"role-secret-fixture","expiration":1_893_456_000_123u64}}),
        json!({"roleCredentials":{"accessKeyId":"ASIAFIXTURE","secretAccessKey":"role-secret-fixture","sessionToken":"role-session-fixture","expiration":1}}),
        json!({"padding":"remote-secret-fixture".repeat(10_000)}),
    ] {
        let files = Files::new();
        let (url, server) = serve(vec![(200, value)]);
        let plan = files.plan(false, &url);
        files.write(&plan, &token(timestamp() / 1000 + 3600));
        no_secrets(&error(resolve(&plan, &CancellationToken::new())));
        server.join().unwrap();
    }
}
#[test]
fn redirects_are_not_followed_and_remote_error_body_is_never_echoed() {
    let files = Files::new();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let plan = files.plan(false, &format!("http://{}", listener.local_addr().unwrap()));
    files.write(&plan, &token(timestamp() / 1000 + 3600));
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        read_request(&mut stream);
        stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: https://remote-secret-fixture.invalid/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
    });
    assert_eq!(
        error(resolve(&plan, &CancellationToken::new())),
        "AWS SSO credential endpoint returned HTTP 302"
    );
    server.join().unwrap();
}
#[test]
fn endpoint_validation_rejects_external_http_credentials_queries_and_fragment() {
    let files = Files::new();
    for url in [
        "http://fixture.invalid",
        "https://user:remote-secret-fixture@example.invalid",
        "https://fixture.invalid?token=remote-secret-fixture",
        "https://fixture.invalid#remote-secret-fixture",
    ] {
        let plan = files.plan(false, url);
        no_secrets(&error(resolve(&plan, &CancellationToken::new())));
    }
}

#[cfg(windows)]
fn dacl_sddl(path: &Path) -> String {
    let descriptor = file_dacl(path).unwrap();
    let mut text = std::ptr::null_mut();
    assert_ne!(
        unsafe {
            ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor.as_ptr().cast_mut().cast(),
                1,
                DACL_SECURITY_INFORMATION,
                &mut text,
                std::ptr::null_mut(),
            )
        },
        0
    );
    let mut length = 0;
    unsafe {
        while *text.add(length) != 0 {
            length += 1;
        }
    }
    let result = String::from_utf16(unsafe { std::slice::from_raw_parts(text, length) }).unwrap();
    unsafe {
        LocalFree(text.cast());
    }
    result
}
#[cfg(windows)]
#[test]
fn windows_protected_dacl_is_present_before_secret_write_and_preserved_after_refresh() {
    let files = Files::new();
    let (url, server) = serve(vec![(200, renewed()), (200, role())]);
    let plan = files.plan(true, &url);
    files.write(&plan, &token(1));
    // Owner Rights and SYSTEM only; inheritance is explicitly disabled.
    let sddl: Vec<u16> = "D:P(A;;FA;;;OW)(A;;FA;;;SY)"
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut descriptor = std::ptr::null_mut();
    assert_ne!(
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        },
        0
    );
    assert_ne!(
        unsafe {
            SetFileSecurityW(
                wide(&cache_path(&plan)).as_ptr(),
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                descriptor,
            )
        },
        0
    );
    unsafe {
        LocalFree(descriptor);
    }
    let expected = dacl_sddl(&cache_path(&plan));
    assert!(expected.starts_with("D:P"));
    let temporary = files.0.join("acl-empty.tmp");
    let file = create_temporary(&cache_path(&plan), &temporary).unwrap();
    // CreateFileW denied sharing intentionally; close only to inspect the empty
    // file's DACL, before any token JSON has ever been written to it.
    drop(file);
    assert_eq!(std::fs::read(&temporary).unwrap(), b"");
    let temporary_dacl = dacl_sddl(&temporary);
    // Windows CreateFile adds its automatic-inheritance bookkeeping flag. It
    // does not add ACEs or remove protection; compare effective access exactly.
    assert_eq!(temporary_dacl.replacen("D:PAI(", "D:P(", 1), expected);
    std::fs::remove_file(&temporary).unwrap();
    resolve(&plan, &CancellationToken::new()).unwrap();
    server.join().unwrap();
    let final_dacl = dacl_sddl(&cache_path(&plan));
    println!("SSO_SOURCE_DACL={expected}\nSSO_EMPTY_TEMP_DACL={temporary_dacl}\nSSO_FINAL_DACL={final_dacl}");
    assert_eq!(final_dacl.replacen("D:PAI(", "D:P(", 1), expected);
}

#[cfg(windows)]
#[test]
fn recovery_backup_restores_only_missing_target_and_preserves_external_changes() {
    let files = Files::new();
    let original = files.0.join("recovery-original.json");
    let backup = files.0.join("recovery-backup.json");
    let bytes = b"original-fixture";
    std::fs::write(&backup, bytes).unwrap();
    recover_backup(&original, &backup, bytes).unwrap();
    assert_eq!(std::fs::read(&original).unwrap(), bytes);
    assert!(!backup.exists());
    std::fs::write(&original, b"external-login-fixture").unwrap();
    std::fs::write(&backup, bytes).unwrap();
    assert_eq!(
        recover_backup(&original, &backup, bytes).unwrap_err(),
        "AWS SSO cache replacement failed; recovery backup retained"
    );
    assert_eq!(std::fs::read(&original).unwrap(), b"external-login-fixture");
    assert_eq!(std::fs::read(&backup).unwrap(), bytes);
    std::fs::remove_file(&original).unwrap();
    std::fs::write(&backup, b"unverified-other-fixture").unwrap();
    assert!(recover_backup(&original, &backup, bytes).is_err());
    assert!(!original.exists());
    assert_eq!(std::fs::read(&backup).unwrap(), b"unverified-other-fixture");
}
#[cfg(windows)]
#[test]
fn windows_sharing_violation_keeps_old_cache_and_cleans_private_temp() {
    let files = Files::new();
    let plan = files.plan(true, "http://127.0.0.1:9");
    let original = files.write(&plan, &token(1));
    let cached = read_cache(&cache_path(&plan), &plan, &CancellationToken::new()).unwrap();
    let held = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(cache_path(&plan))
        .unwrap();
    assert!(commit_cache(
        &cache_path(&plan),
        &plan,
        &cached,
        &token(timestamp() / 1000 + 3600),
        &CancellationToken::new()
    )
    .is_err());
    drop(held);
    assert_eq!(std::fs::read(cache_path(&plan)).unwrap(), original);
    assert_eq!(std::fs::read_dir(&files.0).unwrap().count(), 1);
}

#[test]
fn readers_wait_for_real_sidecar_publication_gap_and_bind_the_new_token() {
    let files = Files::new();
    let (url, server) = serve(vec![(200, role())]);
    let plan = files.plan(true, &url);
    let path = cache_path(&plan);
    let before = files.write(&plan, &token(1));
    let lock = FileLock::acquire(&path, &CancellationToken::new()).unwrap();
    let backup = files.0.join("publication.backup");
    std::fs::rename(&path, &backup).unwrap();
    assert!(!path.exists());
    assert!(local_lock(&path).is_ok());
    let (ready, started) = mpsc::channel();
    let (sender, received) = mpsc::channel();
    let workers: Vec<_> = [false, true]
        .into_iter()
        .map(|fingerprint_only| {
            let plan = plan.clone();
            let ready = ready.clone();
            let sender = sender.clone();
            thread::spawn(move || {
                ready.send(()).unwrap();
                let result = if fingerprint_only {
                    fingerprint(&plan, &CancellationToken::new())
                } else {
                    resolve(&plan, &CancellationToken::new()).map(|result| {
                        assert_eq!(result.credentials.access_key_id, "ASIAFIXTURE");
                        result.source_fingerprint.unwrap()
                    })
                };
                sender.send(result).unwrap();
            })
        })
        .collect();
    for _ in 0..2 {
        started.recv_timeout(Duration::from_secs(2)).unwrap();
    }
    assert!(matches!(
        received.recv_timeout(Duration::from_millis(100)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    assert_eq!(std::fs::read(&backup).unwrap(), before);
    let mut published = token(timestamp() / 1000 + 3600);
    published["accessToken"] = json!("new-access-fixture");
    let bytes = files.write(&plan, &published);
    let expected = token_fingerprint(&plan, &bytes);
    drop(lock);
    for _ in 0..2 {
        assert_eq!(
            received
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap(),
            expected
        );
    }
    for worker in workers {
        worker.join().unwrap();
    }
    assert_eq!(
        server.join().unwrap()[0].headers["x-amz-sso_bearer_token"],
        "new-access-fixture"
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert!(!sidecar(&path).exists());
}

#[test]
fn publication_gap_wait_is_cancellable_without_mutating_the_owned_cache_or_lock() {
    for fingerprint_only in [false, true] {
        let files = Files::new();
        let plan = files.plan(true, "http://127.0.0.1:9");
        let path = cache_path(&plan);
        let before = files.write(&plan, &token(1));
        let lock = FileLock::acquire(&path, &CancellationToken::new()).unwrap();
        let backup = files.0.join("publication.backup");
        std::fs::rename(&path, &backup).unwrap();
        let signal = CancellationToken::new();
        let copy = signal.clone();
        let child_plan = plan.clone();
        let (ready, started) = mpsc::channel();
        let (sender, received) = mpsc::channel();
        let worker = thread::spawn(move || {
            ready.send(()).unwrap();
            let result = if fingerprint_only {
                fingerprint(&child_plan, &copy)
            } else {
                resolve(&child_plan, &copy).map(|result| result.credentials.access_key_id)
            };
            sender.send(result).unwrap();
        });
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(
            received.recv_timeout(Duration::from_millis(50)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        signal.cancel();
        assert_eq!(
            received
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap_err(),
            "Operation aborted"
        );
        worker.join().unwrap();
        assert!(!path.exists());
        assert_eq!(std::fs::read(&backup).unwrap(), before);
        assert_eq!(
            std::fs::read(sidecar(&path)).unwrap(),
            lock.owner.as_bytes()
        );
        drop(lock);
        std::fs::rename(&backup, &path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
}

#[test]
fn publication_wait_rejects_nonregular_or_unbounded_sidecars_before_any_request() {
    for directory in [false, true] {
        let files = Files::new();
        let plan = files.plan(true, "http://127.0.0.1:9");
        let original = files.write(&plan, &token(timestamp() / 1000 + 3600));
        let lock = sidecar(&cache_path(&plan));
        if directory {
            std::fs::create_dir(&lock).unwrap();
        } else {
            std::fs::write(&lock, [b'x'; 257]).unwrap();
        }
        assert_eq!(
            error(resolve(&plan, &CancellationToken::new())),
            "Invalid AWS SSO cache lock file"
        );
        assert_eq!(
            fingerprint(&plan, &CancellationToken::new()).unwrap_err(),
            "Invalid AWS SSO cache lock file"
        );
        assert_eq!(std::fs::read(cache_path(&plan)).unwrap(), original);
        if directory {
            assert!(lock.is_dir());
            std::fs::remove_dir(&lock).unwrap();
        } else {
            assert_eq!(std::fs::read(&lock).unwrap(), [b'x'; 257]);
        }
    }
}

#[test]
fn cross_process_fixture_child() {
    let Some(config) = std::env::var_os("AGENT_CORE_SSO_FIXTURE_CHILD") else {
        return;
    };
    let config: Value = serde_json::from_str(config.to_str().unwrap()).unwrap();
    let files = PathBuf::from(config["cache_dir"].as_str().unwrap());
    // Fixed temporary cache and loopback configuration, never HOME/default AWS.
    let plan = SsoPlan {
        start_url: START.into(),
        region: "us-west-2".into(),
        account_id: "123456789012".into(),
        role_name: config["role"].as_str().unwrap().into(),
        session_name: Some(SESSION.into()),
        cache_dir: files,
        endpoint: config["endpoint"].as_str().unwrap().into(),
        oidc_endpoint: config["endpoint"].as_str().unwrap().into(),
    };
    let result = resolve(&plan, &CancellationToken::new()).unwrap();
    assert_eq!(result.credentials.access_key_id, "ASIAFIXTURE");
}
#[test]
fn separate_processes_share_sidecar_rotation_without_lost_refresh() {
    let files = Files::new();
    let (url, server) = serve(vec![(200, renewed()), (200, role()), (200, role())]);
    let plan = files.plan(true, &url);
    files.write(&plan, &token(1));
    let mut children:Vec<_>=(0..2).map(|index|{
        let mut command=Command::new(std::env::current_exe().unwrap());
        command.args(["--exact","provider::bedrock::credentials::sso::tests::cross_process_fixture_child","--nocapture"])
            .env_clear().env("AGENT_CORE_SSO_FIXTURE_CHILD",json!({"cache_dir":files.0,"endpoint":url,"role":format!("fixture-process-{index}")}).to_string())
            .stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        if let Some(root)=std::env::var_os("SystemRoot"){command.env("SystemRoot",root);}
        command.spawn().unwrap()
    }).collect();
    for mut child in children.drain(..) {
        let started = Instant::now();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                let output = child.wait_with_output().unwrap();
                assert!(
                    status.success(),
                    "child stderr: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
                break;
            }
            if started.elapsed() > Duration::from_secs(8) {
                child.kill().unwrap();
                panic!("fixture child exceeded deadline");
            }
            thread::sleep(Duration::from_millis(25));
        }
    }
    println!("SSO_SEPARATE_PROCESSES=2; EXIT_STATUS=0,0; OIDC_ROTATIONS=1");
    let requests = server.join().unwrap();
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.method == "POST")
            .count(),
        1
    );
    let value: Value = serde_json::from_slice(&std::fs::read(cache_path(&plan)).unwrap()).unwrap();
    assert_eq!(value["refreshToken"], "new-refresh-fixture");
    assert_eq!(std::fs::read_dir(&files.0).unwrap().count(), 1);
}
