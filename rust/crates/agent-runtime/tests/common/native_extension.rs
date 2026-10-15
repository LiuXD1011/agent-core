use agent_runtime::extension_process::NativeExtensionManifest;
use agent_runtime::session::Session;
use serde_json::json;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};
#[cfg(windows)]
use windows_sys::Win32::Foundation::CloseHandle;
#[cfg(windows)]
use windows_sys::Win32::System::Threading::{
    OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
};

pub fn fixture_executable() -> &'static Path {
    static EXECUTABLE: OnceLock<PathBuf> = OnceLock::new();
    EXECUTABLE.get_or_init(|| {
        let directory = std::env::temp_dir().join(format!(
            "agent-native-extension-compiler-{}",
            Session::default().id
        ));
        fs::create_dir_all(&directory).unwrap();
        let output = directory.join(if cfg!(windows) {
            "fixture.exe"
        } else {
            "fixture"
        });
        let compiler = std::env::var_os("RUSTC")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("rustc"));
        let result = Command::new(compiler)
            .args(["--edition=2021", "-O"])
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/native_extension_fixture.rs"))
            .arg("-o")
            .arg(&output)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "Fixture compiler: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        output
    })
}

pub struct Fixture {
    pub path: PathBuf,
    pub manifest: PathBuf,
}
impl Fixture {
    pub fn new(args: &[&str]) -> Self {
        let path =
            std::env::temp_dir().join(format!("agent-native-fixture-{}", Session::default().id));
        fs::create_dir_all(&path).unwrap();
        let executable = path.join(if cfg!(windows) {
            "native fixture.exe"
        } else {
            "native fixture"
        });
        fs::copy(fixture_executable(), &executable).unwrap();
        let manifest = path.join("extension.json");
        fs::write(&manifest,serde_json::to_vec(&json!({"id":"fixture","executable":executable.file_name().unwrap().to_str().unwrap(),"args":args,"requestTimeoutMs":1000,"maxFrameBytes":1_048_576})).unwrap()).unwrap();
        Self { path, manifest }
    }
    pub fn manifest(&self) -> NativeExtensionManifest {
        NativeExtensionManifest::load(&self.manifest).unwrap()
    }
    pub fn session(&self) -> Session {
        Session {
            workspace: self.path.clone(),
            ..Default::default()
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub fn wait_pid(path: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(value) = fs::read_to_string(path) {
            if let Ok(pid) = value.trim().parse() {
                return pid;
            }
        }
        assert!(Instant::now() < deadline, "Fixture pid was not written");
        thread::sleep(Duration::from_millis(5));
    }
}
pub fn assert_gone(pid: u32) {
    #[cfg(windows)]
    {
        // OpenProcess may still find the terminated object's handle, but its
        // signaled state proves that both active requests and pipe owners ended.
        let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if !process.is_null() {
            let status = unsafe { WaitForSingleObject(process, 2000) };
            unsafe { CloseHandle(process) };
            assert_eq!(status, 0, "fixture process {pid} remains live");
        }
    }
    #[cfg(unix)]
    {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Path::new(&format!("/proc/{pid}")).exists() {
            // An orphan may briefly remain a zombie after its owned group dies.
            if fs::read_to_string(format!("/proc/{pid}/stat"))
                .is_ok_and(|stat| stat.split_whitespace().nth(2) == Some("Z"))
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "fixture process {pid} remains live"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
}
