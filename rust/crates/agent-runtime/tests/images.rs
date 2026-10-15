//! Actual native HTTP against offline fixtures, including an independent old-adapter oracle.
use agent_core::types::{CancellationToken, Content, StopReason};
use agent_runtime::images::{
    generate_images, image_model, image_models, images_payload, parse_image_catalog,
    parse_images_response, AssistantImages, ImagesApi, ImagesContext, ImagesHooks, ImagesModel,
    ImagesOptions, ImagesRegistry, NoImagesHooks,
};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

fn oracle() -> Value {
    serde_json::from_str(include_str!("fixtures/openrouter-images.json")).unwrap()
}
fn model() -> ImagesModel {
    serde_json::from_value(oracle()["model"].clone()).unwrap()
}
fn context() -> ImagesContext {
    serde_json::from_value(oracle()["context"].clone()).unwrap()
}
fn options() -> ImagesOptions {
    ImagesOptions {
        api_key: "offline-image-key".into(),
        ..Default::default()
    }
}
fn accept(listener: &TcpListener) -> TcpStream {
    listener.set_nonblocking(true).unwrap();
    let start = Instant::now();
    loop {
        match listener.accept() {
            Ok((socket, _)) => {
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                return socket;
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    && start.elapsed() < Duration::from_secs(3) =>
            {
                thread::sleep(Duration::from_millis(5))
            }
            Err(error) => panic!("offline accept: {error}"),
        }
    }
}
fn request(socket: &mut TcpStream) -> (String, String, Value) {
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    let (head, length) = loop {
        let size = socket.read(&mut chunk).unwrap();
        assert!(size > 0);
        bytes.extend_from_slice(&chunk[..size]);
        if let Some(at) = bytes.windows(4).position(|p| p == b"\r\n\r\n") {
            let header = String::from_utf8_lossy(&bytes[..at]);
            let length = header
                .lines()
                .find_map(|line| {
                    line.split_once(':')
                        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .map(|(_, v)| v.trim().parse::<usize>().unwrap())
                })
                .unwrap();
            break (at + 4, length);
        }
        assert!(bytes.len() < 100_000);
    };
    while bytes.len() < head + length {
        let size = socket.read(&mut chunk).unwrap();
        assert!(size > 0);
        bytes.extend_from_slice(&chunk[..size]);
    }
    let header = String::from_utf8(bytes[..head].to_vec()).unwrap();
    let path = header
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .to_owned();
    (
        path,
        header,
        serde_json::from_slice(&bytes[head..head + length]).unwrap(),
    )
}
fn reply(socket: &mut TcpStream, status: u16, headers: &str, body: &[u8]) {
    write!(socket,"HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n", body.len()).unwrap();
    let _ = socket.write_all(body);
    let _ = socket.flush();
}

#[test]
fn native_image_catalog_keeps_all_52_models_and_metadata_without_node() {
    assert_eq!(image_models().len(), 52);
    assert!(image_models()
        .iter()
        .all(|m| m.provider == "openrouter" && m.api == "openrouter-images"));
    for m in image_models() {
        assert_eq!(image_model(&m.provider, &m.id).unwrap().name, m.name);
    }
    assert!(image_model("missing", "missing").is_none());
    assert!(parse_image_catalog("{}").is_err());
    let mut snapshot: Value = serde_json::from_str(agent_runtime::images::IMAGE_SNAPSHOT).unwrap();
    let models = snapshot["providers"]["openrouter"].as_object_mut().unwrap();
    let first = models.values_mut().next().unwrap();
    first["id"] = json!("mismatched");
    assert!(parse_image_catalog(&snapshot.to_string()).is_err());
}

#[test]
fn payload_and_response_match_actual_original_adapter_oracle() {
    let data = oracle();
    assert_eq!(
        images_payload(&model(), &context()).unwrap(),
        data["payload"]
    );
    let response = parse_images_response(&model(), &data["response"]).unwrap();
    let mut actual = serde_json::to_value(&response).unwrap();
    actual.as_object_mut().unwrap().remove("timestamp");
    actual.as_object_mut().unwrap().remove("errorMessage");
    assert_eq!(actual, data["result"]);
    assert_eq!(response.output.len(), 3); // A remote image URL is not fetched or returned.
    assert_eq!(response.usage.as_ref().unwrap().tokens.cache_read, 5);
    let mut image_only = model();
    image_only.output = vec!["image".into()];
    assert_eq!(
        images_payload(&image_only, &context()).unwrap()["modalities"],
        json!(["image"])
    );
    assert!(images_payload(
        &model(),
        &ImagesContext {
            input: vec![Content::Thinking {
                thinking: "not an image prompt".into()
            }]
        }
    )
    .is_err());
}

struct Hooks {
    payload: usize,
    response: usize,
}
impl ImagesHooks for Hooks {
    fn on_payload(&mut self, payload: &mut Value, _: &ImagesModel) -> Result<(), String> {
        self.payload += 1;
        payload["image_config"] = json!({"aspect_ratio":"16:9"});
        Ok(())
    }
    fn on_response(
        &mut self,
        status: u16,
        headers: &BTreeMap<String, String>,
        _: &ImagesModel,
    ) -> Result<(), String> {
        self.response += 1;
        assert_eq!(status, 200);
        assert_eq!(headers["x-offline-response"], "native");
        Ok(())
    }
}

#[test]
fn actual_native_http_uses_api_key_payload_hooks_and_header_overrides() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut m = model();
    m.base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    m.headers
        .insert("X-Remove".into(), Some("must-not-send".into()));
    let worker = thread::spawn(move || {
        let mut socket = accept(&listener);
        let (path, headers, payload) = request(&mut socket);
        assert_eq!(path, "/v1/chat/completions");
        let lower = headers.to_ascii_lowercase();
        assert!(lower.contains("authorization: bearer offline-image-key"));
        assert!(lower.contains("x-model: override"));
        assert!(!lower.contains("x-remove"));
        assert_eq!(payload["image_config"]["aspect_ratio"], "16:9");
        reply(
            &mut socket,
            200,
            "X-Offline-Response: native\r\n",
            oracle()["response"].to_string().as_bytes(),
        );
    });
    let options = ImagesOptions {
        headers: BTreeMap::from([
            ("x-model".into(), Some("override".into())),
            ("x-remove".into(), None),
        ]),
        ..options()
    };
    let mut hooks = Hooks {
        payload: 0,
        response: 0,
    };
    let result = ImagesRegistry::default().generate_images(
        &m,
        &context(),
        &options,
        &CancellationToken::new(),
        &mut hooks,
    );
    worker.join().unwrap();
    assert_eq!(result.stop_reason, StopReason::Stop);
    assert_eq!((hooks.payload, hooks.response), (1, 1));
    assert_eq!(
        result.response_id.as_deref(),
        Some("image-response-fixture")
    );
}

#[test]
fn retry_is_opt_in_and_respects_server_hints_without_echoing_error_bodies() {
    for (hint, retries, expected) in [("true", 1, 2), ("false", 1, 1), ("true", 0, 1)] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut m = model();
        m.base_url = format!("http://{}", listener.local_addr().unwrap());
        let worker = thread::spawn(move || {
            for attempt in 0..expected {
                let mut socket = accept(&listener);
                request(&mut socket);
                if attempt == 0 {
                    reply(
                        &mut socket,
                        429,
                        &format!("X-Should-Retry: {hint}\r\nRetry-After-Ms: 1\r\n"),
                        br#"{"error":{"message":"secret-failure-body"}}"#,
                    );
                } else {
                    reply(
                        &mut socket,
                        200,
                        "",
                        oracle()["response"].to_string().as_bytes(),
                    );
                }
            }
        });
        let result = generate_images(
            &m,
            &context(),
            &ImagesOptions {
                max_retries: retries,
                ..options()
            },
            &CancellationToken::new(),
        );
        worker.join().unwrap();
        assert_eq!(
            result.stop_reason,
            if expected == 2 {
                StopReason::Stop
            } else {
                StopReason::Error
            }
        );
        assert!(!serde_json::to_string(&result)
            .unwrap()
            .contains("secret-failure-body"));
    }
}

#[test]
fn too_large_server_delay_is_rejected_and_cancel_closes_header_or_body_stall() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut m = model();
    m.base_url = format!("http://{}", listener.local_addr().unwrap());
    let worker = thread::spawn(move || {
        let mut socket = accept(&listener);
        request(&mut socket);
        reply(&mut socket, 429, "Retry-After: 61\r\n", b"secret");
    });
    let result = generate_images(
        &m,
        &context(),
        &ImagesOptions {
            max_retries: 1,
            ..options()
        },
        &CancellationToken::new(),
    );
    worker.join().unwrap();
    assert!(result.error_message.unwrap().contains("delay exceeds"));
    for partial_body in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut m = model();
        m.base_url = format!("http://{}", listener.local_addr().unwrap());
        let (ready, accepted) = mpsc::channel();
        let signal = CancellationToken::new();
        let cancel = signal.clone();
        let worker = thread::spawn(move || {
            let mut socket = accept(&listener);
            request(&mut socket);
            if partial_body {
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n{{").unwrap();
                socket.flush().unwrap();
            }
            ready.send(()).unwrap();
            let mut byte = [0];
            assert_eq!(socket.read(&mut byte).unwrap(), 0);
        });
        let interrupt = thread::spawn(move || {
            accepted.recv_timeout(Duration::from_secs(3)).unwrap();
            cancel.cancel();
        });
        let start = Instant::now();
        let result = generate_images(&m, &context(), &options(), &signal);
        worker.join().unwrap();
        interrupt.join().unwrap();
        assert!(start.elapsed() < Duration::from_secs(2));
        assert_eq!(result.stop_reason, StopReason::Aborted);
    }
}

#[test]
fn local_validation_precancel_and_unregistered_api_have_no_http_side_effects() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut m = model();
    m.base_url = format!("http://{}", listener.local_addr().unwrap());
    assert_eq!(
        generate_images(
            &m,
            &context(),
            &ImagesOptions::default(),
            &CancellationToken::new()
        )
        .stop_reason,
        StopReason::Error
    );
    let signal = CancellationToken::new();
    signal.cancel();
    assert_eq!(
        generate_images(&m, &context(), &options(), &signal).stop_reason,
        StopReason::Aborted
    );
    m.api = "unregistered".into();
    assert_eq!(
        generate_images(&m, &context(), &options(), &CancellationToken::new()).stop_reason,
        StopReason::Error
    );
    listener.set_nonblocking(true).unwrap();
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert!(!format!("{:?}", options()).contains("offline-image-key"));
}

struct NativeImages;
impl ImagesApi for NativeImages {
    fn generate_images(
        &self,
        model: &ImagesModel,
        _: &ImagesContext,
        _: &ImagesOptions,
        _: &CancellationToken,
        _: &mut dyn ImagesHooks,
    ) -> AssistantImages {
        parse_images_response(
            model,
            &json!({"choices":[{"message":{"content":"native registered API"}}]}),
        )
        .unwrap()
    }
}
#[test]
fn native_registry_supports_custom_apis_and_sync_generation_inside_tokio() {
    let mut registry = ImagesRegistry::default();
    registry
        .register("custom-images", Arc::new(NativeImages))
        .unwrap();
    let mut m = model();
    m.api = "custom-images".into();
    let result = registry.generate_images(
        &m,
        &context(),
        &options(),
        &CancellationToken::new(),
        &mut NoImagesHooks,
    );
    assert!(matches!(&result.output[0],Content::Text{text} if text=="native registered API"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let mut m = model();
        m.base_url = "invalid".into();
        assert_eq!(
            generate_images(&m, &context(), &options(), &CancellationToken::new()).stop_reason,
            StopReason::Error
        );
    });
}
