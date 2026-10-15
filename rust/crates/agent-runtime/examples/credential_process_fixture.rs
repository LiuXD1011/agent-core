//! Standalone offline credential_process executable; intentionally uses only std.

use std::io::{self, Write};
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("success");
    for arg in &args {
        if let Some(path) = arg.strip_prefix("--pid=") {
            std::fs::write(path, std::process::id().to_string()).unwrap();
        }
        if let Some(path) = arg.strip_prefix("--record=") {
            let text = format!(
                "{}\n{}",
                std::env::current_dir().unwrap().display(),
                args.join("\n")
            );
            std::fs::write(path, text).unwrap();
        }
    }
    if matches!(mode, "background-exit" | "background-hang") {
        let pid_path = args
            .iter()
            .find_map(|arg| arg.strip_prefix("--child-pid="))
            .unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .arg("child-hang")
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        #[cfg(windows)]
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        let child = command.spawn().unwrap();
        std::fs::write(pid_path, child.id().to_string()).unwrap();
        // The surrounding runner's job/group owns this intentionally live child.
        drop(child);
    }
    if matches!(mode, "hang" | "child-hang" | "background-hang") {
        loop {
            thread::sleep(Duration::from_secs(60));
        }
    }
    match mode {
        "invalid-json" => print!("secret-output {{ invalid JSON"),
        "invalid-utf8" => io::stdout().write_all(&[0xff, 0xfe]).unwrap(),
        "version" => print!(
            "{{\"Version\":2,\"AccessKeyId\":\"AKIAFIXTURE\",\"SecretAccessKey\":\"secret-output\"}}"
        ),
        "version-string" => print!(
            "{{\"Version\":\"1\",\"AccessKeyId\":\"AKIAFIXTURE\",\"SecretAccessKey\":\"secret-output\"}}"
        ),
        "version-float" => print!(
            "{{\"Version\":1.0,\"AccessKeyId\":\"AKIAFIXTURE\",\"SecretAccessKey\":\"secret-output\"}}"
        ),
        "field-type" => print!(
            "{{\"Version\":1,\"AccessKeyId\":\"AKIAFIXTURE\",\"SecretAccessKey\":123}}"
        ),
        "token-null" => print!(
            "{{\"Version\":1,\"AccessKeyId\":\"AKIAFIXTURE\",\"SecretAccessKey\":\"secret-output\",\"SessionToken\":null}}"
        ),
        "expiration-null" => print!(
            "{{\"Version\":1,\"AccessKeyId\":\"AKIAFIXTURE\",\"SecretAccessKey\":\"secret-output\",\"Expiration\":null}}"
        ),
        "expiration-invalid" => print!(
            "{{\"Version\":1,\"AccessKeyId\":\"AKIAFIXTURE\",\"SecretAccessKey\":\"secret-output\",\"Expiration\":\"2030-02-30T00:00:00Z\"}}"
        ),
        "expired" => print!(
            "{{\"Version\":1,\"AccessKeyId\":\"AKIAFIXTURE\",\"SecretAccessKey\":\"secret-output\",\"Expiration\":\"2000-01-01T00:00:00Z\"}}"
        ),
        "header" => print!(
            "{{\"Version\":1,\"AccessKeyId\":\"AKIAFIXTURE\",\"SecretAccessKey\":\"secret-output\\ninvalid\"}}"
        ),
        "empty" => print!(
            "{{\"Version\":1,\"AccessKeyId\":\"\",\"SecretAccessKey\":\"secret-output\"}}"
        ),
        "stdout-flood" | "stderr-flood" => {
            let buffer = [b'x'; 8192];
            let mut writer: Box<dyn Write> = if mode == "stdout-flood" {
                Box::new(io::stdout())
            } else {
                Box::new(io::stderr())
            };
            for _ in 0..256 {
                if writer.write_all(&buffer).is_err() {
                    return;
                }
            }
        }
        "error" => {
            eprintln!("stderr-secret-token secret-output command-secret");
            std::process::exit(7);
        }
        "static" => print!(
            "{{\"Version\":1,\"AccessKeyId\":\"AKIAFIXTURE\",\"SecretAccessKey\":\"secret-output\"}}"
        ),
        _ => {
            if mode == "stderr-secret" {
                eprintln!("stderr-secret-token secret-output command-secret");
            }
            println!(
                "  {{\"Version\":1,\"AccessKeyId\":\"AKIAFIXTURE\",\"SecretAccessKey\":\"secret-output\",\"SessionToken\":\"session-output\",\"Expiration\":\"2099-01-01T00:00:00Z\",\"AccountId\":\"123456789012\",\"CredentialScope\":\"ignored-extra\"}}"
            );
        }
    }
    io::stdout().flush().unwrap();
}
