use super::*;
use agent_core::types::timestamp;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

type Request = (String, BTreeMap<String, String>, String);
fn request(stream: &mut TcpStream) -> Request {
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
fn response(stream: &mut TcpStream, status: u16, body: &str) {
    write!(
        stream,
        "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
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
                        assert!(started.elapsed() < Duration::from_secs(6));
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            requests.push(request(&mut stream));
            response(&mut stream, status, &body);
        }
        requests
    });
    (base, worker)
}
fn creds(key: &str) -> String {
    json!({"Code":"Success","AccessKeyId":key,"SecretAccessKey":"fixture-secret","Token":"fixture-session","Expiration":"2030-01-01T00:00:00Z"}).to_string()
}
fn temp() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../../target/aws-metadata-fixtures")
        .join(format!(
            "{}-{}-{}",
            std::process::id(),
            timestamp(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
    fs::create_dir_all(&path).unwrap();
    path
}
fn resolve(
    options: &BedrockOptions,
    env: &[(&str, String)],
    signal: &CancellationToken,
) -> Result<ResolvedBedrock, String> {
    let env: BTreeMap<String, String> = env
        .iter()
        .map(|(key, value)| ((*key).into(), value.clone()))
        .collect();
    options.resolve_using(
        "https://bedrock-runtime.us-east-1.amazonaws.com",
        "fixture-model",
        "",
        signal,
        &|name| env.get(name).cloned(),
    )
}
#[test]
fn ecs_full_uri_token_precedence_and_expiring_credentials_are_cached() {
    let (base, worker) = server(vec![(200, creds("METADATAKEY"))]);
    let env = [
        (
            "AWS_CONTAINER_CREDENTIALS_FULL_URI",
            format!("{base}/credentials?fixture=1"),
        ),
        (
            "AWS_CONTAINER_CREDENTIALS_RELATIVE_URI",
            "/never-called".into(),
        ),
        (
            "AWS_CONTAINER_AUTHORIZATION_TOKEN",
            "fixture-authorization".into(),
        ),
        (
            "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
            "absent-token-must-not-be-read".into(),
        ),
    ];
    let options = BedrockOptions::default();
    let signal = CancellationToken::new();
    let result = resolve(&options, &env, &signal).unwrap();
    assert_eq!(result.credentials.unwrap().access_key_id, "METADATAKEY");
    assert!(result.credentials_expiration.unwrap() > timestamp() / 1000);
    assert_eq!(
        resolve(&options, &env, &signal)
            .unwrap()
            .credentials
            .unwrap()
            .access_key_id,
        "METADATAKEY"
    );
    let requests = worker.join().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].0.starts_with("GET /credentials?fixture=1"));
    assert_eq!(requests[0].1["authorization"], "fixture-authorization");
}
#[test]
fn ecs_token_file_changes_during_request_do_not_relabel_prior_credentials() {
    let directory = temp();
    let token = directory.join("token");
    fs::write(&token, "fixture-token-zero").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let copy = token.clone();
    let worker = std::thread::spawn(move || {
        let mut requests = vec![];
        for index in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            requests.push(request(&mut stream));
            if index == 0 {
                fs::write(&copy, "fixture-token-one").unwrap();
            }
            response(
                &mut stream,
                200,
                &creds(if index == 0 { "TOKENZERO" } else { "TOKENONE" }),
            );
        }
        requests
    });
    let env = [
        (
            "AWS_CONTAINER_CREDENTIALS_FULL_URI",
            format!("{base}/credentials"),
        ),
        (
            "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
            token.to_string_lossy().into(),
        ),
    ];
    let options = BedrockOptions::default();
    let signal = CancellationToken::new();
    assert_eq!(
        resolve(&options, &env, &signal)
            .unwrap()
            .credentials
            .unwrap()
            .access_key_id,
        "TOKENZERO"
    );
    assert_eq!(
        resolve(&options, &env, &signal)
            .unwrap()
            .credentials
            .unwrap()
            .access_key_id,
        "TOKENONE"
    );
    let requests = worker.join().unwrap();
    assert_eq!(requests[0].1["authorization"], "fixture-token-zero");
    assert_eq!(requests[1].1["authorization"], "fixture-token-one");
}
#[test]
fn ecs_errors_retry_the_same_source_without_env_downgrade_or_secret_echo() {
    let (base, worker) = server(vec![
        (503, "fixture-secret-remote".into()),
        (200, creds("RETRYKEY")),
    ]);
    let env = [(
        "AWS_CONTAINER_CREDENTIALS_FULL_URI",
        format!("{base}/credentials"),
    )];
    assert_eq!(
        resolve(&BedrockOptions::default(), &env, &CancellationToken::new())
            .unwrap()
            .credentials
            .unwrap()
            .access_key_id,
        "RETRYKEY"
    );
    assert_eq!(worker.join().unwrap().len(), 2);
}
#[test]
fn imdsv2_actual_token_role_and_credentials_paths_and_headers() {
    let (base, worker) = server(vec![
        (200, "fixture-imds-token".into()),
        (200, "FixtureRole\n".into()),
        (200, creds("IMDSKEY")),
    ]);
    let env = [
        ("AWS_EC2_METADATA_SERVICE_ENDPOINT", base),
        ("AWS_EC2_METADATA_V1_DISABLED", "true".into()),
    ];
    let options = BedrockOptions::default();
    let result = resolve(&options, &env, &CancellationToken::new()).unwrap();
    assert_eq!(result.credentials.unwrap().access_key_id, "IMDSKEY");
    let requests = worker.join().unwrap();
    assert!(requests[0].0.starts_with("PUT /latest/api/token"));
    assert_eq!(
        requests[0].1["x-aws-ec2-metadata-token-ttl-seconds"],
        "21600"
    );
    assert!(requests[1]
        .0
        .starts_with("GET /latest/meta-data/iam/security-credentials/ "));
    assert!(requests[2]
        .0
        .starts_with("GET /latest/meta-data/iam/security-credentials/FixtureRole"));
    assert_eq!(
        requests[2].1["x-aws-ec2-metadata-token"],
        "fixture-imds-token"
    );
}
#[test]
fn installed_sdk_metadata_oracle_captures_real_provider_paths_and_precedence() {
    let oracle: Value =
        serde_json::from_str(include_str!("../tests/fixtures/aws-metadata.json")).unwrap();
    assert_eq!(oracle["realCloudCalls"], 0);
    assert_eq!(oracle["httpSockets"], 4);
    assert_eq!(
        oracle["cases"]["ecs"]["requests"][0]["path"],
        "/ecs?fixture=1"
    );
    assert_eq!(
        oracle["cases"]["ecs"]["requests"][0]["authorization"],
        "fixture-auth"
    );
    assert_eq!(oracle["cases"]["ec2"]["requests"][0]["method"], "PUT");
    assert_eq!(oracle["cases"]["ec2"]["requests"][0]["ttl"], "21600");
    assert_eq!(
        oracle["cases"]["ec2"]["requests"][2]["imdsToken"],
        "fixture-imds-token"
    );
    assert_eq!(
        oracle["cases"]["roleEnvironment"]["source"]["accessKeyId"],
        "SDKENVKEY"
    );
}
#[test]
fn imdsv1_fallback_is_explicitly_gated_and_token_400_is_not_downgraded() {
    let (base, worker) = server(vec![
        (404, String::new()),
        (200, "FixtureRole".into()),
        (200, creds("V1KEY")),
    ]);
    let env = [("AWS_EC2_METADATA_SERVICE_ENDPOINT", base)];
    assert_eq!(
        resolve(&BedrockOptions::default(), &env, &CancellationToken::new())
            .unwrap()
            .credentials
            .unwrap()
            .access_key_id,
        "V1KEY"
    );
    let requests = worker.join().unwrap();
    assert!(!requests[1].1.contains_key("x-aws-ec2-metadata-token"));
    for status in [400, 404] {
        let (base, worker) = server(vec![(status, "fixture-secret".into())]);
        let env = [
            ("AWS_EC2_METADATA_SERVICE_ENDPOINT", base),
            ("AWS_EC2_METADATA_V1_DISABLED", "true".into()),
        ];
        let error = resolve(&BedrockOptions::default(), &env, &CancellationToken::new())
            .err()
            .unwrap();
        assert!(!error.contains("fixture-secret"));
        assert!(error.contains(if status == 400 { "400" } else { "disabled" }));
        assert_eq!(worker.join().unwrap().len(), 1);
    }
}
#[test]
fn metadata_sources_host_boundaries_and_disable_flags_validate_without_network() {
    let options = BedrockOptions::default();
    for host in [
        "http://example.invalid/credentials",
        "file:///secret",
        "http://user:secret@127.0.0.1/credentials",
    ] {
        let env = [("AWS_CONTAINER_CREDENTIALS_FULL_URI", host.into())];
        assert!(resolve(&options, &env, &CancellationToken::new()).is_err());
    }
    let env = [("AWS_EC2_METADATA_DISABLED", "true".into())];
    assert!(resolve(&options, &env, &CancellationToken::new())
        .err()
        .unwrap()
        .contains("disabled"));
    let env = [(
        "AWS_EC2_METADATA_SERVICE_ENDPOINT_MODE",
        "invalid-mode".into(),
    )];
    assert!(resolve(&options, &env, &CancellationToken::new())
        .err()
        .unwrap()
        .contains("mode"));
}
#[test]
fn named_environment_source_assumes_role_with_actual_sigv4_and_keeps_expiration() {
    let xml="<AssumeRoleResponse><AssumeRoleResult><Credentials><AccessKeyId>ROLEKEY</AccessKeyId><SecretAccessKey>fixture-role-secret</SecretAccessKey><SessionToken>fixture-role-session</SessionToken><Expiration>2030-01-01T00:00:00Z</Expiration></Credentials></AssumeRoleResult></AssumeRoleResponse>";
    let (base, worker) = server(vec![(200, xml.into())]);
    let directory = temp();
    let config = directory.join("config");
    fs::write(&config,"[profile qa]\nrole_arn=arn:aws:iam::123456789012:role/fixture\ncredential_source=Environment\nregion=us-east-1\n").unwrap();
    let options = BedrockOptions {
        profile: "qa".into(),
        config_file: Some(config),
        sts_endpoint: base,
        ..Default::default()
    };
    let env = [
        ("AWS_ACCESS_KEY_ID", "ENVKEY".into()),
        ("AWS_SECRET_ACCESS_KEY", "fixture-env-secret".into()),
        ("AWS_SESSION_TOKEN", "fixture-env-token".into()),
        ("AWS_CREDENTIAL_EXPIRATION", "2030-01-01T00:00:00Z".into()),
    ];
    let result = resolve(&options, &env, &CancellationToken::new()).unwrap();
    assert_eq!(result.credentials.unwrap().access_key_id, "ROLEKEY");
    let requests = worker.join().unwrap();
    assert!(requests[0].1["authorization"].contains("Credential=ENVKEY/"));
    assert_eq!(requests[0].1["x-amz-security-token"], "fixture-env-token");
    assert!(requests[0].2.contains("Action=AssumeRole"));
}
#[test]
fn named_ecs_source_then_sts_is_native_and_invalid_named_source_is_not_ambient() {
    let xml="<AssumeRoleResponse><AssumeRoleResult><Credentials><AccessKeyId>ROLEKEY</AccessKeyId><SecretAccessKey>fixture-role-secret</SecretAccessKey><SessionToken>fixture-role-session</SessionToken><Expiration>2030-01-01T00:00:00Z</Expiration></Credentials></AssumeRoleResult></AssumeRoleResponse>";
    let (base, worker) = server(vec![(200, creds("ECSKEY")), (200, xml.into())]);
    let directory = temp();
    let config = directory.join("config");
    fs::write(&config,"[profile qa]\nrole_arn=arn:aws:iam::123456789012:role/fixture\ncredential_source=EcsContainer\nregion=us-east-1\n").unwrap();
    let options = BedrockOptions {
        profile: "qa".into(),
        config_file: Some(config.clone()),
        sts_endpoint: base.clone(),
        ..Default::default()
    };
    let env = [
        (
            "AWS_CONTAINER_CREDENTIALS_FULL_URI",
            format!("{base}/credentials"),
        ),
        ("AWS_ACCESS_KEY_ID", "UNUSEDENV".into()),
        ("AWS_SECRET_ACCESS_KEY", "unused-env-secret".into()),
    ];
    assert_eq!(
        resolve(&options, &env, &CancellationToken::new())
            .unwrap()
            .credentials
            .unwrap()
            .access_key_id,
        "ROLEKEY"
    );
    let requests = worker.join().unwrap();
    assert!(requests[1].1["authorization"].contains("Credential=ECSKEY/"));
    fs::write(config,"[profile qa]\nrole_arn=arn:aws:iam::123456789012:role/fixture\ncredential_source=invalid\n").unwrap();
    let error = resolve(&options, &env, &CancellationToken::new())
        .err()
        .unwrap();
    assert!(error.contains("credential_source"));
    assert!(!error.contains("unused-env-secret"));
}
#[test]
fn metadata_cancellation_stops_before_any_v1_fallback_or_bedrock_request() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (sent, received) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        request(&mut stream);
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
        listener.set_nonblocking(true).unwrap();
        assert!(listener.accept().is_err());
    });
    let env = [("AWS_EC2_METADATA_SERVICE_ENDPOINT", base)];
    let signal = CancellationToken::new();
    let copy = signal.clone();
    let caller = std::thread::spawn(move || resolve(&BedrockOptions::default(), &env, &copy));
    received.recv_timeout(Duration::from_secs(3)).unwrap();
    let started = Instant::now();
    signal.cancel();
    assert_eq!(caller.join().unwrap().err().unwrap(), "Operation aborted");
    assert!(started.elapsed() < Duration::from_secs(1));
    worker.join().unwrap();
}
