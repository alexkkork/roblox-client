//! Run the actual desktop executable, including a relocated copy and local IPC.
#![cfg(all(feature = "rust-host", any(unix, windows)))]

use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_owned()
}

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let mut bytes = [0; 8];
        rove_client_core::entropy::fill(&mut bytes).unwrap();
        let name: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let path = std::env::temp_dir().join(format!("rv-{name}"));
        private_directory(&path);
        Self(path)
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn private_directory(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(path).unwrap();
    }
    #[cfg(windows)]
    drop(rove_client_core::windows_private::create_directory(path).unwrap());
}

struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn executable_runs_the_official_vm_and_reports_resident_memory() {
    let output = Command::new(env!("CARGO_BIN_EXE_rove"))
        .args(["run", "--snapshot"])
        .arg(root().join("tests/fixtures/world_snapshot.json"))
        .arg("--script")
        .arg(root().join("examples/rust-host-smoke.luau"))
        .arg("--json")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str::<Value>(line).ok())
        .expect("final JSON result");
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["values"][0]["passed"], 12.0, "{result}");
    assert!(result["resident_bytes_at_end"].as_u64().unwrap() > 0);
    assert_eq!(result["live_game_session"], false);
}

#[test]
fn relocated_browser_and_console_share_one_runtime_and_close_cleanly() {
    let temporary = Directory::new();
    let package = temporary.0.join("package");
    let unrelated = temporary.0.join("unrelated");
    fs::create_dir(&package).unwrap();
    fs::create_dir(&unrelated).unwrap();
    fs::create_dir(package.join("ui")).unwrap();
    fs::write(
        package.join("ui/local-session.html"),
        "<meta name=\"console-key\" content=\"__LAUNCHER_KEY__\"><p>relocated preview</p>",
    )
    .unwrap();
    let executable = package.join(if cfg!(windows) { "rove.exe" } else { "rove" });
    fs::copy(env!("CARGO_BIN_EXE_rove"), &executable).unwrap();
    let secrets = temporary.0.join("secrets");
    private_directory(&secrets);
    let endpoint = secrets.join("ipc");
    let child = Command::new(executable)
        .current_dir(&unrelated)
        .args(["serve", "--snapshot"])
        .arg(root().join("tests/fixtures/world_snapshot.json"))
        .args(["--port", "0", "--serve-seconds", "20", "--socket"])
        .arg(&endpoint)
        .arg("--lease-root")
        .arg(&temporary.0)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let mut running = Running(child);
    let output = running.0.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(output).lines().map_while(Result::ok) {
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                if value["event"] == "ready" || value["ok"] == false {
                    let _ = sender.send(value);
                }
            }
        }
    });
    let ready = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("desktop ready event");
    assert_eq!(ready["event"], "ready", "desktop startup failed: {ready}");
    let url = ready["url"].as_str().unwrap();
    let base = url.strip_suffix("/session").unwrap();
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(5))
        .build();
    let page = agent.get(url).call().unwrap().into_string().unwrap();
    assert!(
        page.contains("relocated preview"),
        "must load assets beside the executable"
    );
    let key = page
        .split("content=\"")
        .nth(1)
        .unwrap()
        .split('"')
        .next()
        .unwrap();
    assert_eq!(key.len(), 32);
    assert!(!page.contains("__LAUNCHER_KEY__"));
    let result = agent.post(&format!("{base}/api/runtime/execute"))
        .set("Origin", base).set("X-Console-Key", key).set("Content-Type", "application/json")
        .send_string(&json!({"source": "local key=crypt.generatekey(); local c,iv=crypt.encrypt('hello\\0windows',key); assert(crypt.decrypt(c,key,iv)=='hello\\0windows'); assert(Random.new():NextNumber()>=0); return 42", "timeout_ms":1000}).to_string())
        .unwrap().into_string().unwrap();
    let result: Value = serde_json::from_str(&result).unwrap();
    assert_eq!(result["ok"], true, "{result}");
    let job = result["job_id"].as_u64().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = rove_client_core::console::send_request(
            &endpoint,
            json!({"op":"status","job_id":job}),
            Duration::from_secs(3),
        )
        .unwrap();
        assert_eq!(status["ok"], true, "{status}");
        if status["completed"] == true {
            assert_eq!(status["values"][0], 42.0, "{status}");
            break;
        }
        assert!(Instant::now() < deadline, "{status}");
        thread::sleep(Duration::from_millis(10));
    }
    let leave = rove_client_core::console::send_request(
        &endpoint,
        json!({"op":"leave"}),
        Duration::from_secs(3),
    )
    .unwrap();
    assert_eq!(leave["ok"], true);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = running.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "desktop did not close");
        thread::sleep(Duration::from_millis(10));
    }
    reader.join().unwrap();
    assert!(
        !endpoint.exists(),
        "local endpoint should be removed on exit"
    );
    drop(rove_client_core::console::SessionLease::acquire(&temporary.0).unwrap());
}
