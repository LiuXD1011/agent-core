#[path = "common/native_extension.rs"]
mod fixture;
use agent_core::types::CancellationToken;
use agent_runtime::extension_process::{NativeExtensionManifest, NativeProcess};
use fixture::{assert_gone, wait_pid, Fixture};
use serde_json::json;
use std::fs;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn direct_argv_utf8_and_stderr_flood_are_isolated_and_bounded() {
    let fixture = Fixture::new(&[]);
    let argv = fixture.path.join("argv.jsonl");
    let mut manifest = fixture.manifest();
    manifest.args = vec![
        "--stderr-flood".into(),
        format!("--argv={}", argv.display()),
        "literal space ; & $() \"quoted\" 中文🙂".into(),
    ];
    let process = NativeProcess::spawn(manifest, &fixture.path, &CancellationToken::new()).unwrap();
    let payload = json!({"text":"中文🙂\nquotes\""});
    let result = process
        .request(
            "echo",
            payload.clone(),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap();
    assert_eq!(result, payload);
    assert_eq!(process.stderr_text().len(), 102_400);
    let args = fs::read_to_string(argv)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<String>(line).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(args[2], "literal space ; & $() \"quoted\" 中文🙂");
}

#[test]
fn malformed_utf8_oversized_crash_and_wrong_version_poison_only_the_extension() {
    for flag in [
        "--malformed",
        "--invalid-utf8",
        "--oversize",
        "--crash",
        "--wrong-version",
    ] {
        let fixture = Fixture::new(&[flag]);
        let mut manifest = fixture.manifest();
        manifest.max_frame_bytes = 1024;
        let process =
            NativeProcess::spawn(manifest, &fixture.path, &CancellationToken::new()).unwrap();
        let start = Instant::now();
        assert!(
            process
                .request("echo", json!({}), &CancellationToken::new(), &mut |_| {})
                .is_err(),
            "{flag}"
        );
        assert!(start.elapsed() < Duration::from_secs(2));
        assert!(process
            .request("echo", json!({}), &CancellationToken::new(), &mut |_| {})
            .is_err());
    }
}

#[test]
fn remote_business_failure_does_not_poison_a_valid_process() {
    let fixture = Fixture::new(&[]);
    let process =
        NativeProcess::spawn(fixture.manifest(), &fixture.path, &CancellationToken::new()).unwrap();
    let error = process
        .request("error", json!({}), &CancellationToken::new(), &mut |_| {})
        .unwrap_err();
    assert!(error.contains("fixture error"));
    assert_eq!(
        process
            .request(
                "echo",
                json!({"after":true}),
                &CancellationToken::new(),
                &mut |_| {}
            )
            .unwrap(),
        json!({"after":true})
    );
}

#[test]
fn blocked_stdin_write_obeys_deadline_and_kills_its_pipe_owner() {
    let fixture = Fixture::new(&[]);
    let pid_file = fixture.path.join("pid");
    let mut manifest = fixture.manifest();
    manifest.args = vec!["--no-read".into(), format!("--pid={}", pid_file.display())];
    manifest.request_timeout_ms = 150;
    let process = NativeProcess::spawn(manifest, &fixture.path, &CancellationToken::new()).unwrap();
    let pid = wait_pid(&pid_file);
    let start = Instant::now();
    let error = process
        .request(
            "echo",
            json!({"text":"x".repeat(900_000)}),
            &CancellationToken::new(),
            &mut |_| {},
        )
        .unwrap_err();
    assert!(error.contains("timed out"), "{error}");
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_gone(pid);
}

#[test]
fn cancellation_interrupts_a_stalled_reply_and_terminates_the_process() {
    let fixture = Fixture::new(&[]);
    let pid_file = fixture.path.join("pid");
    let mut manifest = fixture.manifest();
    manifest.args = vec!["--hang".into(), format!("--pid={}", pid_file.display())];
    manifest.request_timeout_ms = 30_000;
    let process = NativeProcess::spawn(manifest, &fixture.path, &CancellationToken::new()).unwrap();
    let pid = wait_pid(&pid_file);
    let signal = CancellationToken::new();
    let cancel = signal.clone();
    let worker = thread::spawn(move || {
        thread::sleep(Duration::from_millis(75));
        cancel.cancel();
    });
    let start = Instant::now();
    let error = process
        .request("echo", json!({}), &signal, &mut |_| {})
        .unwrap_err();
    worker.join().unwrap();
    assert!(error.contains("aborted"));
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_gone(pid);
}

#[test]
fn cancellation_while_waiting_for_serialized_transport_does_not_block() {
    let fixture = Fixture::new(&["--hang"]);
    let mut manifest = fixture.manifest();
    manifest.request_timeout_ms = 30_000;
    let process =
        Arc::new(NativeProcess::spawn(manifest, &fixture.path, &CancellationToken::new()).unwrap());
    let primary = process.clone();
    let owner = thread::spawn(move || {
        primary.request("echo", json!({}), &CancellationToken::new(), &mut |_| {})
    });
    while !process.is_busy() {
        thread::yield_now();
    }
    let signal = CancellationToken::new();
    let cancel = signal.clone();
    let worker = thread::spawn(move || {
        thread::sleep(Duration::from_millis(50));
        cancel.cancel();
    });
    let start = Instant::now();
    assert!(process
        .request("echo", json!({}), &signal, &mut |_| {})
        .unwrap_err()
        .contains("aborted"));
    assert!(start.elapsed() < Duration::from_secs(2));
    process.shutdown();
    assert!(owner.join().unwrap().is_err());
    worker.join().unwrap();
}

#[test]
fn drop_and_parent_exit_kill_descendants_that_inherit_pipes() {
    for parent_exits in [false, true] {
        let fixture = Fixture::new(&[]);
        let pid_file = fixture.path.join("child.pid");
        let mut manifest = fixture.manifest();
        manifest.args = vec![format!("--child={}", pid_file.display())];
        if parent_exits {
            manifest.args.push("--exit-after-init".into());
        }
        let process =
            NativeProcess::spawn(manifest, &fixture.path, &CancellationToken::new()).unwrap();
        let pid = wait_pid(&pid_file);
        if parent_exits {
            let _ = process.request(
                "initialize",
                json!({"extensionId":"fixture"}),
                &CancellationToken::new(),
                &mut |_| {},
            );
            thread::sleep(Duration::from_millis(20));
            let start = Instant::now();
            assert!(process
                .request("echo", json!({}), &CancellationToken::new(), &mut |_| {})
                .is_err());
            assert!(start.elapsed() < Duration::from_secs(2));
        }
        drop(process);
        assert_gone(pid);
    }
}

#[test]
fn manifests_are_explicit_bounded_and_relative_to_the_manifest() {
    let fixture = Fixture::new(&[]);
    assert_eq!(fixture.session().workspace, fixture.path);
    let manifest = fixture.manifest();
    assert!(manifest.executable.is_absolute());
    let mut value = serde_json::to_value(manifest).unwrap();
    value["schema"] = json!("old-js-extension");
    fs::write(&fixture.manifest, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(NativeExtensionManifest::load(&fixture.manifest).is_err());
    fs::write(&fixture.manifest, vec![b' '; 65_537]).unwrap();
    assert!(NativeExtensionManifest::load(&fixture.manifest)
        .unwrap_err()
        .contains("64KB"));
}

#[test]
fn oversized_outgoing_request_does_not_destroy_valid_process() {
    let fixture = Fixture::new(&[]);
    let mut manifest = fixture.manifest();
    manifest.max_frame_bytes = 256;
    let process = NativeProcess::spawn(manifest, &fixture.path, &CancellationToken::new()).unwrap();
    assert!(process
        .request(
            "echo",
            json!({"text":"x".repeat(1000)}),
            &CancellationToken::new(),
            &mut |_| {}
        )
        .unwrap_err()
        .contains("frame exceeds"));
    assert_eq!(
        process
            .request("echo", json!({}), &CancellationToken::new(), &mut |_| {})
            .unwrap(),
        json!({})
    );
}
