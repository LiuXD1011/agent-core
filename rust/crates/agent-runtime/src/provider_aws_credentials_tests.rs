//! Every credential resolver uses an injected environment and temporary files.
use super::*;
use crate::provider::{
    aws_timestamp, sign_aws_v4, ModelProvider, ProviderMode, ProviderOptions, ProviderSettings,
    SigV4Parameters,
};
use agent_core::agent_loop::Provider;
use agent_core::types::timestamp;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::sync::{mpsc, Arc, Barrier, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const ROLE: &str = "arn:aws:iam::123456789012:role/native-fixture";
const SOURCE_KEY: &str = "AKIDFIXTURE";
const SOURCE_SECRET: &str = "source-secret-fixture";
const SOURCE_TOKEN: &str = "source-session-fixture";
const TEMP_KEY: &str = "ASIAFIXTURE";
const TEMP_SECRET: &str = "temporary-secret-fixture";
const TEMP_TOKEN: &str = "temporary-session-fixture";
const WEB_TOKEN: &str = "offline.jwt.fixture-token";
const SDK_QUERY_ORACLE: &str = include_str!("../tests/fixtures/aws-sts-query.json");

fn verify_sdk_query(request: &Request, action: &str) {
    let oracle: Value = serde_json::from_str(SDK_QUERY_ORACLE).unwrap();
    let case = oracle["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["action"] == action)
        .unwrap();
    assert_eq!(
        serde_json::to_value(request.fields()).unwrap(),
        case["fields"]
    );
    assert_eq!(request.path, case["path"].as_str().unwrap());
    assert_eq!(
        request.headers["content-type"],
        case["content_type"].as_str().unwrap()
    );
    assert_eq!(
        request.headers.contains_key("authorization"),
        case["has_authorization"].as_bool().unwrap()
    );
}

struct Files(PathBuf);
impl Files {
    fn new(config: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "native-sts-fixture-{}",
            crate::session::Session::default().id
        ));
        fs::create_dir(&path).unwrap();
        fs::write(path.join("config"), config).unwrap();
        fs::write(path.join("credentials"),format!("[source]\naws_access_key_id={SOURCE_KEY}\naws_secret_access_key={SOURCE_SECRET}\naws_session_token={SOURCE_TOKEN}\n")).unwrap();
        fs::write(path.join("token"), WEB_TOKEN).unwrap();
        Self(path)
    }
    fn role() -> Self {
        Self::new(&format!("[profile qa]\nregion=us-west-2\nrole_arn={ROLE}\nsource_profile=source\nrole_session_name=fixture-session\nexternal_id=fixture-external\nduration_seconds=1800\n"))
    }
    fn options(&self, endpoint: &str) -> BedrockOptions {
        BedrockOptions {
            profile: "qa".into(),
            credentials_file: Some(self.0.join("credentials")),
            config_file: Some(self.0.join("config")),
            sts_endpoint: endpoint.into(),
            ..Default::default()
        }
    }
}
impl Drop for Files {
    fn drop(&mut self) {
        let directory = fs::canonicalize(&self.0).unwrap();
        assert_eq!(
            directory.parent(),
            Some(fs::canonicalize(std::env::temp_dir()).unwrap().as_path())
        );
        assert!(self
            .0
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("native-sts-fixture-"));
        fs::remove_dir_all(directory).unwrap();
    }
}
fn resolve(
    options: &BedrockOptions,
    signal: &CancellationToken,
) -> Result<ResolvedBedrock, String> {
    resolve_env(options, &[], signal)
}
fn resolve_env(
    options: &BedrockOptions,
    env: &[(&str, &str)],
    signal: &CancellationToken,
) -> Result<ResolvedBedrock, String> {
    let env: BTreeMap<_, _> = env
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect();
    options.resolve_using(
        "https://bedrock-runtime.us-east-1.amazonaws.com",
        "fixture-model",
        "",
        signal,
        &|key| env.get(key).cloned(),
    )
}
fn xml(action: &str, key: &str, expiration: &str) -> Vec<u8> {
    format!("<{}Response xmlns=\"https://sts.amazonaws.com/doc/2011-06-15/\"><{}Result><Credentials><AccessKeyId>{key}</AccessKeyId><SecretAccessKey>{TEMP_SECRET}</SecretAccessKey><SessionToken>{TEMP_TOKEN}</SessionToken><Expiration>{expiration}</Expiration></Credentials><AssumedRoleUser><Arn>arn:aws:sts::123456789012:assumed-role/native-fixture/fixture-session</Arn></AssumedRoleUser></{}Result><ResponseMetadata><RequestId>offline</RequestId></ResponseMetadata></{}Response>",action,action,action,action).into_bytes()
}
fn success(action: &str) -> Vec<u8> {
    xml(action, TEMP_KEY, "2030-01-01T00:00:00Z")
}
struct Request {
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}
impl Request {
    fn fields(&self) -> BTreeMap<String, String> {
        let mut url = Url::parse("https://fixture.invalid").unwrap();
        url.set_query(Some(std::str::from_utf8(&self.body).unwrap()));
        url.query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect()
    }
}
fn accept(listener: &TcpListener) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let now = Instant::now();
    loop {
        match listener.accept() {
            Ok((socket, _)) => {
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                return socket;
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    && now.elapsed() < Duration::from_secs(3) =>
            {
                thread::sleep(Duration::from_millis(5))
            }
            Err(error) => panic!("Fixture accept: {error}"),
        }
    }
}
fn request(socket: &mut TcpStream) -> Request {
    let mut bytes = vec![];
    let mut chunk = [0; 4096];
    let (head, length) = loop {
        let count = socket.read(&mut chunk).unwrap();
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
    while bytes.len() < head + length {
        let count = socket.read(&mut chunk).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&chunk[..count]);
    }
    let headers = String::from_utf8_lossy(&bytes[..head]);
    Request {
        path: headers
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .into(),
        headers: headers
            .lines()
            .skip(1)
            .filter_map(|line| {
                line.split_once(':')
                    .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
            })
            .collect(),
        body: bytes[head..head + length].to_vec(),
    }
}
fn respond(socket: &mut TcpStream, status: u16, body: &[u8]) {
    write!(socket,"HTTP/1.1 {status} Fixture\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).unwrap();
    let _ = socket.write_all(body);
    let _ = socket.flush();
}
struct Server {
    url: String,
    requests: mpsc::Receiver<Request>,
    worker: thread::JoinHandle<()>,
}
impl Server {
    fn new(responses: Vec<(u16, Vec<u8>)>, delay: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (send, requests) = mpsc::channel();
        let worker = thread::spawn(move || {
            for (status, body) in responses {
                let mut socket = accept(&listener);
                send.send(request(&mut socket)).unwrap();
                thread::sleep(delay);
                respond(&mut socket, status, &body);
            }
        });
        Self {
            url,
            requests,
            worker,
        }
    }
    fn finish(self) -> Vec<Request> {
        self.worker.join().unwrap();
        self.requests.into_iter().collect()
    }
}
fn verify_signature(
    request: &Request,
    base: &str,
    key: &str,
    secret: &str,
    token: &str,
    service: &str,
) {
    let signed = request.headers["authorization"]
        .split("SignedHeaders=")
        .nth(1)
        .unwrap()
        .split(',')
        .next()
        .unwrap();
    let headers = request
        .headers
        .iter()
        .filter(|(name, _)| {
            signed.split(';').any(|part| part == *name)
                && !matches!(
                    name.as_str(),
                    "host" | "x-amz-date" | "x-amz-content-sha256" | "x-amz-security-token"
                )
        })
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect();
    let signature = sign_aws_v4(SigV4Parameters {
        method: "POST",
        url: &Url::parse(&(base.to_owned() + &request.path)).unwrap(),
        body: &request.body,
        headers: &headers,
        credentials: &AwsCredentials {
            access_key_id: key.into(),
            secret_access_key: secret.into(),
            session_token: token.into(),
        },
        region: "us-west-2",
        service,
        timestamp: &request.headers["x-amz-date"],
        checksum_header: true,
    })
    .unwrap();
    assert_eq!(
        request.headers["authorization"],
        signature.headers["authorization"]
    );
    if token.is_empty() {
        assert!(!request.headers.contains_key("x-amz-security-token"));
    } else {
        assert_eq!(request.headers["x-amz-security-token"], token);
    }
}

#[test]
fn native_assume_role_signs_query_with_source_profile_and_uses_temporary_credentials_for_bedrock() {
    let files = Files::role();
    let sts = Server::new(vec![(200, success("AssumeRole"))], Duration::ZERO);
    let options = files.options(&sts.url);
    let result = resolve(&options, &CancellationToken::new()).unwrap();
    assert_eq!(result.credentials_expiration, Some(1_893_456_000));
    let credentials = result.credentials.unwrap();
    assert_eq!(credentials.access_key_id, TEMP_KEY);
    let model = Server::new(
        vec![(
            200,
            include_bytes!("../tests/fixtures/bedrock-converse.eventstream").to_vec(),
        )],
        Duration::ZERO,
    );
    let mut provider = ModelProvider {
        settings: ProviderSettings {
            mode: ProviderMode::AmazonBedrock,
            base_url: model.url.clone(),
            model: "us.anthropic.claude-sonnet-4-fixture-v1:0".into(),
            api_key: String::new(),
            options: ProviderOptions {
                bedrock: BedrockOptions {
                    region: "us-west-2".into(),
                    profile: "offline-explicit".into(),
                    access_key_id: credentials.access_key_id,
                    secret_access_key: credentials.secret_access_key,
                    session_token: credentials.session_token,
                    ..Default::default()
                },
                ..Default::default()
            },
        },
        tool_definitions: vec![],
    };
    let output = provider
        .complete(
            &Context {
                messages: vec![Message::user("fixture")],
                ..Default::default()
            },
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(Message::Assistant(output).text(), "中文回答");
    let request = model.requests.recv_timeout(Duration::from_secs(3)).unwrap();
    verify_signature(
        &request,
        &model.url,
        TEMP_KEY,
        TEMP_SECRET,
        TEMP_TOKEN,
        "bedrock",
    );
    model.finish();
    let request = sts.finish().remove(0);
    verify_sdk_query(&request, "AssumeRole");
    verify_signature(
        &request,
        &options.sts_endpoint,
        SOURCE_KEY,
        SOURCE_SECRET,
        SOURCE_TOKEN,
        "sts",
    );
    let fields = request.fields();
    assert_eq!(fields["Action"], "AssumeRole");
    assert_eq!(fields["Version"], "2011-06-15");
    assert_eq!(fields["RoleArn"], ROLE);
    assert_eq!(fields["RoleSessionName"], "fixture-session");
    assert_eq!(fields["ExternalId"], "fixture-external");
    assert_eq!(fields["DurationSeconds"], "1800");
    assert!(!String::from_utf8_lossy(&request.body).contains(SOURCE_SECRET));
}
#[test]
fn native_role_chain_resolves_inside_out_reuses_expiring_credentials_and_signs_each_hop() {
    let files=Files::new(&format!("[profile qa]\nregion=us-west-2\nrole_arn={ROLE}\nsource_profile=inner\n[profile inner]\nrole_arn=arn:aws:iam::123456789012:role/inner\nsource_profile=source\n"));
    let sts = Server::new(
        vec![
            (200, xml("AssumeRole", "ASIAINNER", "2030-01-01T00:00:00Z")),
            (200, success("AssumeRole")),
        ],
        Duration::ZERO,
    );
    let options = files.options(&sts.url);
    for _ in 0..3 {
        let result = resolve(&options, &CancellationToken::new()).unwrap();
        assert_eq!(result.credentials.unwrap().access_key_id, TEMP_KEY);
    }
    let requests = sts.finish();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].fields()["RoleArn"].ends_with("role/inner"));
    assert_eq!(requests[1].fields()["RoleArn"], ROLE);
    verify_signature(
        &requests[0],
        &options.sts_endpoint,
        SOURCE_KEY,
        SOURCE_SECRET,
        SOURCE_TOKEN,
        "sts",
    );
    verify_signature(
        &requests[1],
        &options.sts_endpoint,
        "ASIAINNER",
        TEMP_SECRET,
        TEMP_TOKEN,
        "sts",
    );
}
#[test]
fn source_profile_cycles_missing_sources_and_depth_fail_before_network() {
    for config in [
        format!("[profile qa]\nrole_arn={ROLE}\nsource_profile=qa\n"),
        format!("[profile qa]\nrole_arn={ROLE}\nsource_profile=missing\n"),
        {
            let mut config = String::new();
            for index in 0..9 {
                config.push_str(&format!(
                    "[profile {}]\nrole_arn={ROLE}\nsource_profile=p{}\n",
                    if index == 0 {
                        "qa".into()
                    } else {
                        format!("p{index}")
                    },
                    index + 1
                ));
            }
            config
        },
    ] {
        let files = Files::new(&config);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let options = files.options(&format!("http://{}", listener.local_addr().unwrap()));
        let error = resolve(&options, &CancellationToken::new()).err().unwrap();
        assert!(error.contains("cycle") || error.contains("chain") || error.contains("required"));
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().err().unwrap().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}
#[test]
fn profile_web_identity_and_injected_environment_web_identity_are_unsigned_and_cache_real_expiry() {
    for environment in [false, true] {
        let files = Files::new("");
        if !environment {
            fs::write(files.0.join("config"),format!("[profile qa]\nregion=us-west-2\nrole_arn={ROLE}\nrole_session_name=web-fixture\nweb_identity_token_file={}\n",files.0.join("token").display())).unwrap();
        } else {
            fs::write(files.0.join("config"), "[default]\nregion=us-west-2\n").unwrap();
        }
        let sts = Server::new(
            vec![(200, success("AssumeRoleWithWebIdentity"))],
            Duration::ZERO,
        );
        let mut options = files.options(&sts.url);
        options.region = "us-west-2".into();
        let path = files.0.join("token").to_string_lossy().into_owned();
        let env = if environment {
            options.profile.clear();
            vec![
                ("AWS_ROLE_ARN", ROLE),
                ("AWS_WEB_IDENTITY_TOKEN_FILE", path.as_str()),
                ("AWS_ROLE_SESSION_NAME", "web-fixture"),
            ]
        } else {
            vec![]
        };
        for _ in 0..2 {
            let result = resolve_env(&options, &env, &CancellationToken::new()).unwrap();
            assert_eq!(result.credentials_expiration, Some(1_893_456_000));
            assert_eq!(result.credentials.unwrap().session_token, TEMP_TOKEN);
        }
        let request = sts.finish().remove(0);
        verify_sdk_query(&request, "AssumeRoleWithWebIdentity");
        assert!(!request.headers.contains_key("authorization"));
        assert!(!request.headers.contains_key("x-amz-security-token"));
        let fields = request.fields();
        assert_eq!(fields["Action"], "AssumeRoleWithWebIdentity");
        assert_eq!(fields["WebIdentityToken"], WEB_TOKEN);
        assert_eq!(fields["RoleSessionName"], "web-fixture");
    }
}
#[test]
fn near_expiry_credentials_refresh_before_use_and_rotated_profile_source_invalidates_cache() {
    let files = Files::role();
    let expiry = aws_timestamp(timestamp() / 1000 + 30).unwrap();
    let sts = Server::new(
        vec![
            (
                200,
                xml(
                    "AssumeRole",
                    "ASIAOLD",
                    &format!(
                        "{}-{}-{}T{}:{}:{}Z",
                        &expiry[..4],
                        &expiry[4..6],
                        &expiry[6..8],
                        &expiry[9..11],
                        &expiry[11..13],
                        &expiry[13..15]
                    ),
                ),
            ),
            (200, success("AssumeRole")),
            (200, xml("AssumeRole", "ASIANEW", "2030-01-01T00:00:00Z")),
        ],
        Duration::ZERO,
    );
    let options = files.options(&sts.url);
    assert_eq!(
        resolve(&options, &CancellationToken::new())
            .unwrap()
            .credentials
            .unwrap()
            .access_key_id,
        "ASIAOLD"
    );
    assert_eq!(
        resolve(&options, &CancellationToken::new())
            .unwrap()
            .credentials
            .unwrap()
            .access_key_id,
        TEMP_KEY
    );
    assert_eq!(
        resolve(&options, &CancellationToken::new())
            .unwrap()
            .credentials
            .unwrap()
            .access_key_id,
        TEMP_KEY
    );
    fs::write(files.0.join("credentials"),"[source]\naws_access_key_id=ROTATEDKEY\naws_secret_access_key=rotated-secret\naws_session_token=rotated-session\n").unwrap();
    assert_eq!(
        resolve(&options, &CancellationToken::new())
            .unwrap()
            .credentials
            .unwrap()
            .access_key_id,
        "ASIANEW"
    );
    let requests = sts.finish();
    assert_eq!(requests.len(), 3);
    verify_signature(
        &requests[2],
        &options.sts_endpoint,
        "ROTATEDKEY",
        "rotated-secret",
        "rotated-session",
        "sts",
    );
}
#[test]
fn web_token_file_rotation_endpoint_and_region_bind_the_cache_identity() {
    let files = Files::new("");
    fs::write(
        files.0.join("config"),
        format!(
            "[profile qa]\nregion=us-west-2\nrole_arn={ROLE}\nweb_identity_token_file={}\n",
            files.0.join("token").display()
        ),
    )
    .unwrap();
    let first = Server::new(
        vec![
            (200, success("AssumeRoleWithWebIdentity")),
            (200, success("AssumeRoleWithWebIdentity")),
            (200, success("AssumeRoleWithWebIdentity")),
        ],
        Duration::ZERO,
    );
    let mut options = files.options(&first.url);
    resolve(&options, &CancellationToken::new()).unwrap();
    fs::write(files.0.join("token"), "rotated.jwt.fixture-token").unwrap();
    resolve(&options, &CancellationToken::new()).unwrap();
    options.region = "eu-west-1".into();
    resolve(&options, &CancellationToken::new()).unwrap();
    let requests = first.finish();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests[1].fields()["WebIdentityToken"],
        "rotated.jwt.fixture-token"
    );
    let second = Server::new(
        vec![(200, success("AssumeRoleWithWebIdentity"))],
        Duration::ZERO,
    );
    options.sts_endpoint = second.url.clone();
    resolve(&options, &CancellationToken::new()).unwrap();
    assert_eq!(second.finish().len(), 1);
}
#[test]
fn simultaneous_same_profile_role_requests_coalesce_one_actual_exchange() {
    let files = Files::role();
    let sts = Server::new(
        vec![(200, success("AssumeRole"))],
        Duration::from_millis(100),
    );
    let options = files.options(&sts.url);
    let barrier = Arc::new(Barrier::new(6));
    let mut workers = vec![];
    for _ in 0..6 {
        let options = options.clone();
        let barrier = barrier.clone();
        workers.push(thread::spawn(move || {
            barrier.wait();
            resolve(&options, &CancellationToken::new())
                .unwrap()
                .credentials
                .unwrap()
                .access_key_id
        }));
    }
    for worker in workers {
        assert_eq!(worker.join().unwrap(), TEMP_KEY);
    }
    assert_eq!(sts.finish().len(), 1);
}
#[test]
fn simultaneous_failed_exchange_is_shared_but_a_later_caller_can_retry() {
    let files = Files::role();
    let sts = Server::new(
        vec![
            (403, b"remote-secret-body".to_vec()),
            (200, success("AssumeRole")),
        ],
        Duration::from_millis(150),
    );
    let options = files.options(&sts.url);
    let barrier = Arc::new(Barrier::new(6));
    let workers: Vec<_> = (0..6)
        .map(|_| {
            let options = options.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                resolve(&options, &CancellationToken::new()).err().unwrap()
            })
        })
        .collect();
    for worker in workers {
        assert!(worker.join().unwrap().contains("403"));
    }
    assert_eq!(
        resolve(&options, &CancellationToken::new())
            .unwrap()
            .credentials
            .unwrap()
            .access_key_id,
        TEMP_KEY
    );
    assert_eq!(sts.finish().len(), 2);
}
#[test]
fn web_identity_cancellation_during_partial_xml_closes_socket_without_returning_credentials() {
    let files = Files::new("");
    fs::write(
        files.0.join("config"),
        format!(
            "[profile qa]\nregion=us-west-2\nrole_arn={ROLE}\nweb_identity_token_file={}\n",
            files.0.join("token").display()
        ),
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (send, accepted) = mpsc::channel();
    let server = thread::spawn(move || {
        let mut socket = accept(&listener);
        let request = request(&mut socket);
        assert!(!request.headers.contains_key("authorization"));
        socket.write_all(b"HTTP/1.1 200 Fixture\r\nContent-Type: text/xml\r\nContent-Length: 9999\r\n\r\n<AssumeRoleWithWebIdentityResponse>").unwrap();
        socket.flush().unwrap();
        send.send(()).unwrap();
        wait_closed(&mut socket);
    });
    let options = files.options(&endpoint);
    let signal = CancellationToken::new();
    let child_signal = signal.clone();
    let worker = thread::spawn(move || resolve(&options, &child_signal).err().unwrap());
    accepted.recv_timeout(Duration::from_secs(3)).unwrap();
    thread::sleep(Duration::from_millis(50));
    let start = Instant::now();
    signal.cancel();
    assert!(worker.join().unwrap().contains("aborted"));
    assert!(start.elapsed() < Duration::from_secs(2));
    server.join().unwrap();
}
fn wait_closed(socket: &mut TcpStream) {
    socket
        .set_read_timeout(Some(Duration::from_millis(25)))
        .unwrap();
    let started = Instant::now();
    let mut byte = [0];
    loop {
        match socket.read(&mut byte) {
            Ok(0) => return,
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => return,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) && started.elapsed() < Duration::from_secs(2) => {}
            other => panic!("STS cancelled connection stayed open: {other:?}"),
        }
    }
}
#[test]
fn cancelled_exchange_closes_stalled_socket_leaves_no_cache_and_next_caller_retries() {
    let files = Files::role();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (send, accepted) = mpsc::channel();
    let server = thread::spawn(move || {
        let mut socket = accept(&listener);
        request(&mut socket);
        send.send(()).unwrap();
        wait_closed(&mut socket);
        let mut next = accept(&listener);
        request(&mut next);
        respond(&mut next, 200, &success("AssumeRole"));
    });
    let options = files.options(&endpoint);
    let signal = CancellationToken::new();
    let child_signal = signal.clone();
    let child_options = options.clone();
    let worker = thread::spawn(move || resolve(&child_options, &child_signal).err().unwrap());
    accepted.recv_timeout(Duration::from_secs(3)).unwrap();
    let start = Instant::now();
    signal.cancel();
    assert!(worker.join().unwrap().contains("aborted"));
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(
        resolve(&options, &CancellationToken::new())
            .unwrap()
            .credentials
            .unwrap()
            .access_key_id,
        TEMP_KEY
    );
    server.join().unwrap();
}
#[test]
fn cancelling_a_singleflight_waiter_does_not_cancel_or_poison_the_owner() {
    let files = Files::role();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (send, accepted) = mpsc::channel();
    let (release, ready) = mpsc::channel();
    let server = thread::spawn(move || {
        let mut socket = accept(&listener);
        request(&mut socket);
        send.send(()).unwrap();
        ready.recv_timeout(Duration::from_secs(3)).unwrap();
        respond(&mut socket, 200, &success("AssumeRole"));
    });
    let options = files.options(&endpoint);
    let owner_options = options.clone();
    let owner = thread::spawn(move || {
        resolve(&owner_options, &CancellationToken::new())
            .unwrap()
            .credentials
            .unwrap()
            .access_key_id
    });
    accepted.recv_timeout(Duration::from_secs(3)).unwrap();
    let signal = CancellationToken::new();
    let child_signal = signal.clone();
    let child_options = options.clone();
    let waiter = thread::spawn(move || resolve(&child_options, &child_signal).err().unwrap());
    thread::sleep(Duration::from_millis(50));
    let start = Instant::now();
    signal.cancel();
    assert!(waiter.join().unwrap().contains("aborted"));
    assert!(start.elapsed() < Duration::from_secs(1));
    release.send(()).unwrap();
    assert_eq!(owner.join().unwrap(), TEMP_KEY);
    assert_eq!(
        resolve(&options, &CancellationToken::new())
            .unwrap()
            .credentials
            .unwrap()
            .access_key_id,
        TEMP_KEY
    );
    server.join().unwrap();
}
#[test]
fn sts_http_xml_expiration_and_size_failures_never_expose_tokens_or_remote_bodies() {
    for (status, body) in [
        (
            403,
            format!("remote-secret {SOURCE_SECRET} {WEB_TOKEN}").into_bytes(),
        ),
        (200, b"<Invalid>temporary-secret-fixture</Invalid>".to_vec()),
        (200, xml("AssumeRole", TEMP_KEY, "2000-01-01T00:00:00Z")),
        (200, vec![b'x'; 65_537]),
    ] {
        let files = Files::role();
        let server = Server::new(vec![(status, body)], Duration::ZERO);
        let error = resolve(&files.options(&server.url), &CancellationToken::new())
            .err()
            .unwrap();
        for secret in [
            SOURCE_SECRET,
            TEMP_SECRET,
            TEMP_TOKEN,
            WEB_TOKEN,
            "remote-secret",
        ] {
            assert!(!error.contains(secret));
        }
        server.finish();
    }
}
#[test]
fn web_token_validation_and_cancelled_preflight_stop_before_network_and_validation_is_pure() {
    let files = Files::new("");
    fs::write(
        files.0.join("config"),
        format!(
            "[profile qa]\nregion=us-west-2\nrole_arn={ROLE}\nweb_identity_token_file={}\n",
            files.0.join("token").display()
        ),
    )
    .unwrap();
    let options = files.options("http://127.0.0.1:1");
    for token in [
        vec![],
        vec![255],
        vec![b'x'; 20_003],
        b"jwt\r\ninjected".to_vec(),
    ] {
        fs::write(files.0.join("token"), token).unwrap();
        assert!(resolve(&options, &CancellationToken::new()).is_err());
    }
    let signal = CancellationToken::new();
    signal.cancel();
    let unread = BedrockOptions {
        config_file: Some(files.0.join("missing")),
        ..options
    };
    assert!(resolve(&unread, &signal).err().unwrap().contains("aborted"));
    unread.validate("http://127.0.0.1:1", "fixture").unwrap();
    for endpoint in [
        "http://remote.invalid",
        "https://user:secret@sts.invalid",
        "https://sts.invalid/?token=secret",
    ] {
        assert!(BedrockOptions {
            sts_endpoint: endpoint.into(),
            ..Default::default()
        }
        .validate("http://127.0.0.1:1", "fixture")
        .is_err());
    }
    assert_eq!(
        credentials::sts_endpoint("", "cn-north-1")
            .unwrap()
            .host_str()
            .unwrap(),
        "sts.cn-north-1.amazonaws.com.cn"
    );
    assert_eq!(
        credentials::sts_endpoint("", "us-gov-west-1")
            .unwrap()
            .host_str()
            .unwrap(),
        "sts.us-gov-west-1.amazonaws.com"
    );
}
#[test]
fn malformed_role_settings_reject_locally_without_downgrading_to_static_profile() {
    for fields in [
        "role_session_name=bad name\n",
        "duration_seconds=899\n",
        "duration_seconds=43201\n",
        "external_id=bad value\n",
        "web_identity_token_file=unread\n",
        "mfa_serial=unread\n",
    ] {
        let files = Files::new(&format!(
            "[profile qa]\nrole_arn={ROLE}\nsource_profile=source\n{fields}"
        ));
        assert!(resolve(
            &files.options("http://127.0.0.1:1"),
            &CancellationToken::new()
        )
        .is_err());
    }
}
#[test]
fn default_static_profile_precedes_env_web_identity_and_bearer_bypasses_role_graph() {
    let files = Files::new("[default]\nregion=us-west-2\n");
    fs::write(
        files.0.join("credentials"),
        format!(
            "[default]\naws_access_key_id={SOURCE_KEY}\naws_secret_access_key={SOURCE_SECRET}\n"
        ),
    )
    .unwrap();
    let mut options = files.options("http://127.0.0.1:1");
    options.profile.clear();
    let result = resolve_env(
        &options,
        &[
            ("AWS_ROLE_ARN", ROLE),
            ("AWS_WEB_IDENTITY_TOKEN_FILE", "must-not-read"),
        ],
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(result.credentials.unwrap().access_key_id, SOURCE_KEY);
    assert!(result.credentials_expiration.is_none());
    fs::write(
        files.0.join("config"),
        format!("[profile qa]\nrole_arn={ROLE}\nsource_profile=qa\n"),
    )
    .unwrap();
    options.profile = "qa".into();
    options.region = "us-west-2".into();
    let result = resolve_env(
        &options,
        &[("AWS_BEARER_TOKEN_BEDROCK", "bearer-fixture")],
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(result.bearer_token.as_deref(), Some("bearer-fixture"));
    assert!(result.credentials.is_none());
    assert!(result.credentials_expiration.is_none());
}

fn credential_executable() -> &'static Path {
    static EXE: OnceLock<PathBuf> = OnceLock::new();
    EXE.get_or_init(|| {
        let path = std::env::temp_dir().join(format!(
            "chain-compiler-{}",
            crate::session::Session::default().id
        ));
        fs::create_dir(&path).unwrap();
        let exe = path.join(if cfg!(windows) {
            "credential fixture.exe"
        } else {
            "credential fixture"
        });
        let mut command = Command::new(std::env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()));
        #[cfg(windows)]
        command.creation_flags(0x0800_0000);
        let output = command
            .args(["--edition=2021", "-O"])
            .arg(
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("examples/credential_process_fixture.rs"),
            )
            .arg("-o")
            .arg(&exe)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        exe
    })
}
fn process_command(files: &Files, mode: &str) -> String {
    let arguments = [
        credential_executable().to_string_lossy().into_owned(),
        mode.into(),
        format!("--record={}", files.0.join("process-record").display()),
    ];
    arguments
        .iter()
        .map(|arg| {
            if cfg!(windows) {
                format!("\"{arg}\"")
            } else {
                format!("'{}'", arg.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
fn cache_token(files: &Files, name: &str, token: &str, modern: bool) -> PathBuf {
    let cache = files.0.join("cache");
    fs::create_dir_all(&cache).unwrap();
    let hash = ring::digest::digest(&ring::digest::SHA1_FOR_LEGACY_USE_ONLY, name.as_bytes());
    let hash: String = hash
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let path = cache.join(format!("{hash}.json"));
    let mut value = json!({"accessToken":token,"expiresAt":"2099-01-01T00:00:00Z","region":"us-east-1","startUrl":"https://portal.example/start","unknown":{"keep":true}});
    if modern {
        value["clientId"] = json!("offline-client");
    }
    fs::write(&path, value.to_string()).unwrap();
    path
}
fn sso_response(key: &str) -> Vec<u8> {
    json!({"roleCredentials":{"accessKeyId":key,"secretAccessKey":"sso-secret","sessionToken":"sso-session","expiration":4070908800000u64}}).to_string().into_bytes()
}
fn verify_model_with_options(options: BedrockOptions, key: &str, secret: &str, token: &str) {
    // Resolve with an injected empty environment, then sign the actual binary
    // model request with those exact credentials. Ambient host credentials are
    // never consulted by the fixture's ModelProvider.
    let resolved = resolve(&options, &CancellationToken::new()).unwrap();
    let credentials = resolved.credentials.unwrap();
    let options = BedrockOptions {
        region: resolved.region,
        access_key_id: credentials.access_key_id,
        secret_access_key: credentials.secret_access_key,
        session_token: credentials.session_token,
        ..options
    };
    let model = Server::new(
        vec![(
            200,
            include_bytes!("../tests/fixtures/bedrock-converse.eventstream").to_vec(),
        )],
        Duration::ZERO,
    );
    let mut provider = ModelProvider {
        settings: ProviderSettings {
            mode: ProviderMode::AmazonBedrock,
            base_url: model.url.clone(),
            model: "us.anthropic.claude-sonnet-4-fixture-v1:0".into(),
            api_key: String::new(),
            options: ProviderOptions {
                bedrock: options,
                ..Default::default()
            },
        },
        tool_definitions: vec![],
    };
    let result = provider
        .complete(
            &Context {
                messages: vec![Message::user("offline")],
                ..Default::default()
            },
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(Message::Assistant(result).text(), "中文回答");
    let request = model.requests.recv_timeout(Duration::from_secs(3)).unwrap();
    verify_signature(&request, &model.url, key, secret, token, "bedrock");
    assert!(!String::from_utf8_lossy(&request.body).contains(secret));
    model.finish();
}

#[test]
fn process_profile_cache_respects_expiration_and_no_expiry_and_model_uses_real_sigv4() {
    for mode in ["success", "static"] {
        let files = Files::new("");
        fs::write(
            files.0.join("config"),
            format!(
                "[profile qa]\nregion=us-west-2\ncredential_process={}\n",
                process_command(&files, mode)
            ),
        )
        .unwrap();
        let options = files.options("http://127.0.0.1:1");
        let first = resolve(&options, &CancellationToken::new()).unwrap();
        assert_eq!(first.credentials.unwrap().access_key_id, "AKIAFIXTURE");
        assert_eq!(first.credentials_expiration.is_some(), mode == "success");
        fs::remove_file(files.0.join("process-record")).unwrap();
        resolve(&options, &CancellationToken::new()).unwrap();
        assert_eq!(files.0.join("process-record").exists(), mode == "static");
        verify_model_with_options(
            options,
            "AKIAFIXTURE",
            "secret-output",
            if mode == "success" {
                "session-output"
            } else {
                ""
            },
        );
    }
}
#[test]
fn process_source_profile_signs_assume_role_and_parent_cache_reuses_actual_credentials() {
    let files = Files::new("");
    fs::write(files.0.join("config"),format!("[profile qa]\nregion=us-west-2\nrole_arn={ROLE}\nsource_profile=process\n[profile process]\ncredential_process={}\n",process_command(&files,"success"))).unwrap();
    let sts = Server::new(vec![(200, success("AssumeRole"))], Duration::ZERO);
    let options = files.options(&sts.url);
    let result = resolve(&options, &CancellationToken::new()).unwrap();
    assert_eq!(result.credentials.unwrap().access_key_id, TEMP_KEY);
    fs::remove_file(files.0.join("process-record")).unwrap();
    resolve(&options, &CancellationToken::new()).unwrap();
    assert!(!files.0.join("process-record").exists());
    let endpoint = sts.url.clone();
    let request = sts.finish().remove(0);
    verify_signature(
        &request,
        &endpoint,
        "AKIAFIXTURE",
        "secret-output",
        "session-output",
        "sts",
    );
}
#[test]
fn complete_static_profiles_skip_process_and_sso_and_malformed_graph_skips_process() {
    for extra in [
        "credential_process=must-not-execute\n",
        "sso_session=must-not-read\nsso_account_id=bad\n",
    ] {
        let files = Files::new(&format!("[profile qa]\nregion=us-west-2\n{extra}"));
        fs::write(
            files.0.join("credentials"),
            format!(
                "[qa]\naws_access_key_id={SOURCE_KEY}\naws_secret_access_key={SOURCE_SECRET}\n"
            ),
        )
        .unwrap();
        assert_eq!(
            resolve(
                &files.options("http://127.0.0.1:1"),
                &CancellationToken::new()
            )
            .unwrap()
            .credentials
            .unwrap()
            .access_key_id,
            SOURCE_KEY
        );
    }
    let files = Files::new("");
    fs::write(
        files.0.join("config"),
        format!(
            "[profile qa]\nrole_arn={ROLE}\nsource_profile=qa\ncredential_process={}\n",
            process_command(&files, "success")
        ),
    )
    .unwrap();
    assert!(resolve(
        &files.options("http://127.0.0.1:1"),
        &CancellationToken::new()
    )
    .err()
    .unwrap()
    .contains("cycle"));
    assert!(!files.0.join("process-record").exists());
}
#[test]
fn legacy_and_named_sso_profiles_bind_cache_region_identity_and_use_signed_bedrock() {
    for modern in [false, true] {
        let files = Files::new(if modern {
            "[profile qa]\nregion=us-west-2\nsso_session=fixture\nsso_account_id=123456789012\nsso_role_name=Reader\n[sso-session \"fixture\"]\nsso_region=us-east-1\nsso_start_url=https://portal.example/start\n"
        } else {
            "[profile qa]\nregion=us-west-2\nsso_account_id=123456789012\nsso_role_name=Reader\nsso_region=us-east-1\nsso_start_url=https://portal.example/start\n"
        });
        let path = cache_token(
            &files,
            if modern {
                "fixture"
            } else {
                "https://portal.example/start"
            },
            "offline-sso-token",
            modern,
        );
        let before = fs::read(&path).unwrap();
        let sso = Server::new(vec![(200, sso_response("ASIASSO"))], Duration::ZERO);
        let options = BedrockOptions {
            sso_cache_dir: Some(files.0.join("cache")),
            sso_endpoint: sso.url.clone(),
            ..files.options("http://127.0.0.1:1")
        };
        assert_eq!(
            resolve(&options, &CancellationToken::new())
                .unwrap()
                .credentials
                .unwrap()
                .access_key_id,
            "ASIASSO"
        );
        verify_model_with_options(options, "ASIASSO", "sso-secret", "sso-session");
        let requests = sso.finish();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].headers["x-amz-sso_bearer_token"],
            "offline-sso-token"
        );
        assert!(!requests[0].headers.contains_key("authorization"));
        let route =
            Url::parse(&("https://fixture.invalid".to_owned() + &requests[0].path)).unwrap();
        assert_eq!(route.path(), "/federation/credentials");
        assert_eq!(
            route
                .query_pairs()
                .collect::<BTreeMap<_, _>>()
                .get("account_id")
                .unwrap(),
            "123456789012"
        );
        assert_eq!(fs::read(path).unwrap(), before);
    }
}
#[test]
fn sso_source_profile_signs_sts_and_uses_bedrock_region_separate_from_sso_region() {
    let files=Files::new(&format!("[profile qa]\nregion=us-west-2\nrole_arn={ROLE}\nsource_profile=login\n[profile login]\nsso_account_id=123456789012\nsso_role_name=Reader\nsso_region=us-east-1\nsso_start_url=https://portal.example/start\n"));
    cache_token(
        &files,
        "https://portal.example/start",
        "offline-sso-token",
        false,
    );
    let sso = Server::new(vec![(200, sso_response("ASIASSO"))], Duration::ZERO);
    let sts = Server::new(vec![(200, success("AssumeRole"))], Duration::ZERO);
    let options = BedrockOptions {
        sso_cache_dir: Some(files.0.join("cache")),
        sso_endpoint: sso.url.clone(),
        ..files.options(&sts.url)
    };
    verify_model_with_options(options, TEMP_KEY, TEMP_SECRET, TEMP_TOKEN);
    let endpoint = sts.url.clone();
    let request = sts.finish().remove(0);
    verify_signature(
        &request,
        &endpoint,
        "ASIASSO",
        "sso-secret",
        "sso-session",
        "sts",
    );
    assert_eq!(
        sso.finish()[0].headers["x-amz-sso_bearer_token"],
        "offline-sso-token"
    );
}
#[test]
fn external_sso_login_during_get_role_never_relabels_old_credentials_as_new_identity() {
    let files=Files::new("[profile qa]\nregion=us-west-2\nsso_account_id=123456789012\nsso_role_name=Reader\nsso_region=us-east-1\nsso_start_url=https://portal.example/start\n");
    let path = cache_token(
        &files,
        "https://portal.example/start",
        "token-before",
        false,
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (accepted, ready) = mpsc::channel();
    let (release, resume) = mpsc::channel();
    let worker = thread::spawn(move || {
        let mut socket = accept(&listener);
        assert_eq!(
            request(&mut socket).headers["x-amz-sso_bearer_token"],
            "token-before"
        );
        accepted.send(()).unwrap();
        resume.recv_timeout(Duration::from_secs(3)).unwrap();
        respond(&mut socket, 200, &sso_response("ASIABEFORE"));
        let mut socket = accept(&listener);
        assert_eq!(
            request(&mut socket).headers["x-amz-sso_bearer_token"],
            "token-after"
        );
        respond(&mut socket, 200, &sso_response("ASIAAFTER"));
    });
    let options = BedrockOptions {
        sso_cache_dir: Some(files.0.join("cache")),
        sso_endpoint: endpoint,
        ..files.options("http://127.0.0.1:1")
    };
    let first_options = options.clone();
    let first = thread::spawn(move || {
        resolve(&first_options, &CancellationToken::new())
            .unwrap()
            .credentials
            .unwrap()
            .access_key_id
    });
    ready.recv_timeout(Duration::from_secs(3)).unwrap();
    let mut changed: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    changed["accessToken"] = json!("token-after");
    fs::write(&path, changed.to_string()).unwrap();
    release.send(()).unwrap();
    assert_eq!(first.join().unwrap(), "ASIABEFORE");
    assert_eq!(
        resolve(&options, &CancellationToken::new())
            .unwrap()
            .credentials
            .unwrap()
            .access_key_id,
        "ASIAAFTER"
    );
    worker.join().unwrap();
    assert_eq!(
        resolve(&options, &CancellationToken::new())
            .unwrap()
            .credentials
            .unwrap()
            .access_key_id,
        "ASIAAFTER"
    );
}
#[test]
fn missing_or_conflicting_sso_config_is_local_and_bearer_bypasses_cache_process_and_profile() {
    for fields in ["sso_session=missing\nsso_account_id=123456789012\nsso_role_name=Reader\n", "sso_account_id=bad\nsso_role_name=Reader\nsso_region=us-east-1\nsso_start_url=https://portal.example/start\n", "sso_session=fixture\nsso_region=eu-west-1\nsso_account_id=123456789012\nsso_role_name=Reader\n[sso-session fixture]\nsso_region=us-east-1\nsso_start_url=https://portal.example/start\n"] {
        let files=Files::new(&format!("[profile qa]\nregion=us-west-2\n{fields}"));
        let listener=TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint=format!("http://{}",listener.local_addr().unwrap());
        let options=BedrockOptions{sso_cache_dir:Some(files.0.join("missing")),sso_endpoint:endpoint,..files.options("http://127.0.0.1:1")};
        assert!(resolve(&options,&CancellationToken::new()).is_err());
        listener.set_nonblocking(true).unwrap();
        assert_eq!(listener.accept().err().unwrap().kind(),std::io::ErrorKind::WouldBlock);
        let bypass=BedrockOptions{region:"us-west-2".into(),config_file:Some(files.0.join("must-not-read")),bearer_token:"offline-bearer".into(),..options};
        assert_eq!(resolve(&bypass,&CancellationToken::new()).unwrap().bearer_token.as_deref(),Some("offline-bearer"));
    }
}
