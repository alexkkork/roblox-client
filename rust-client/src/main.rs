#[cfg(unix)]
use std::os::unix::net::UnixStream;
// Cargo attaches build.rs native link metadata to the package library target.
extern crate headless_luau;

mod codecs;
mod decompiler;
mod host_io;
mod native;
mod network;
mod session;
mod test_limits;

use serde_json::{json, Value};
use std::{
    env,
    fs::File,
    io::{Read, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const MAX_SOURCE: usize = 100 * 1024 * 1024;
const MAX_SNAPSHOT: usize = 8 * 1024 * 1024;
const MAX_FRAME: usize = 1024 * 1024;
const USAGE: &str = "headless-luau --source FILE [--world-snapshot FILE] [--timeout-ms 1..2000] [--memory-mb 8..512] [-- NUMBER ...]\nheadless-luau submit --script FILE [--socket PATH] [--timeout-ms 1..2000]";

#[derive(Debug)]
struct Options {
    submit: bool,
    worker: bool,
    source: PathBuf,
    snapshot: Option<PathBuf>,
    socket: PathBuf,
    timeout_ms: u32,
    memory_mb: u32,
    arguments: Vec<f64>,
}

fn parse(arguments: &[String]) -> Result<Options, String> {
    let submit = arguments.first().is_some_and(|v| v == "submit");
    let mut result = Options {
        submit,
        worker: false,
        source: PathBuf::new(),
        snapshot: None,
        socket: PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("secrets/client-script.sock"),
        timeout_ms: 250,
        memory_mb: 512,
        arguments: Vec::new(),
    };
    if submit {
        result.timeout_ms = 1000;
    }
    let mut seen = std::collections::HashSet::new();
    let mut index = usize::from(submit);
    while index < arguments.len() {
        let flag = &arguments[index];
        index += 1;
        if flag == "--" {
            if submit {
                return Err("submit does not accept numeric arguments".into());
            }
            for arg in &arguments[index..] {
                let n: f64 = arg
                    .parse()
                    .map_err(|_| "arguments must be finite numbers")?;
                if !n.is_finite() {
                    return Err("arguments must be finite numbers".into());
                }
                result.arguments.push(n);
            }
            break;
        }
        if !seen.insert(flag) {
            return Err(format!("duplicate option: {flag}"));
        }
        if flag == "--worker" && !submit {
            result.worker = true;
            continue;
        }
        let value = arguments
            .get(index)
            .ok_or_else(|| format!("missing value for {flag}"))?;
        index += 1;
        match flag.as_str() {
            "--source" if !submit => result.source = value.into(),
            "--script" if submit => result.source = value.into(),
            "--world-snapshot" if !submit => result.snapshot = Some(value.into()),
            "--socket" if submit => result.socket = value.into(),
            "--timeout-ms" => result.timeout_ms = value.parse().map_err(|_| "invalid timeout")?,
            "--memory-mb" if !submit => {
                result.memory_mb = value.parse().map_err(|_| "invalid memory cap")?
            }
            _ => return Err(format!("unknown option: {flag}")),
        }
    }
    if result.source.as_os_str().is_empty() {
        return Err("source file is required".into());
    }
    if !(1..=test_limits::time_ms()).contains(&result.timeout_ms) {
        return Err(format!("timeout must be 1..{} ms", test_limits::time_ms()));
    }
    if !(8..=test_limits::memory_mb()).contains(&result.memory_mb) {
        return Err(format!(
            "memory cap must be 8..{} MiB",
            test_limits::memory_mb()
        ));
    }
    if result.arguments.len() > 64 {
        return Err("at most 64 numeric arguments are supported".into());
    }
    Ok(result)
}

fn read_file(path: &PathBuf, limit: usize) -> Result<Vec<u8>, String> {
    let file = File::open(path).map_err(|e| format!("cannot open input: {e}"))?;
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("input must be a regular file".into());
    }
    let mut data = Vec::new();
    file.take((limit + 1) as u64)
        .read_to_end(&mut data)
        .map_err(|e| e.to_string())?;
    if data.len() > limit {
        return Err(format!("input exceeds {limit} bytes"));
    }
    Ok(data)
}

fn validate_result(data: &[u8], successful_exit: bool) -> Result<Value, String> {
    if data.len() > MAX_FRAME {
        return Err("response exceeds 1 MiB".into());
    }
    let result: Value = serde_json::from_slice(data).map_err(|_| "worker returned invalid JSON")?;
    if !result.is_object() || !result["ok"].is_boolean() || !result["values"].is_array() {
        return Err("worker returned an invalid result".into());
    }
    if result["ok"] == true && !successful_exit {
        return Err("worker exited unsuccessfully but reported success".into());
    }
    Ok(result)
}

struct Worker(Child);
impl Drop for Worker {
    fn drop(&mut self) {
        // Always reap a killed worker, including I/O and validation failures.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn supervise(options: &Options, original_arguments: &[String]) -> Result<Value, String> {
    let mut command = Command::new(env::current_exe().map_err(|e| e.to_string())?);
    command
        .arg("--worker")
        .args(original_arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut worker = Worker(
        command
            .spawn()
            .map_err(|e| format!("cannot start worker: {e}"))?,
    );
    let stdout = worker.0.stdout.take().ok_or("worker has no result pipe")?;
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut data = Vec::new();
        let result = stdout
            .take((MAX_FRAME + 1) as u64)
            .read_to_end(&mut data)
            .map_err(|e| e.to_string())
            .and_then(|_| {
                if data.len() > MAX_FRAME {
                    Err("worker output exceeds 1 MiB".into())
                } else {
                    Ok(data)
                }
            });
        let _ = sender.send(result);
    });
    let deadline = Instant::now() + Duration::from_millis(u64::from(options.timeout_ms) + 2000);
    let mut output = None;
    loop {
        if output.is_none() {
            match receiver.try_recv() {
                Ok(result) => output = Some(result?),
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err("worker result pipe disconnected".into())
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(status) = worker.0.try_wait().map_err(|e| e.to_string())? {
            let data = match output {
                Some(data) => data,
                None => receiver
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .map_err(|_| "worker result pipe did not close")??,
            };
            return validate_result(&data, status.success());
        }
        if Instant::now() >= deadline {
            return Ok(
                json!({"ok":false,"values":[],"error":"worker wall time limit exceeded","timed_out":true}),
            );
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn submit(options: &Options) -> Result<Value, String> {
    let source = String::from_utf8(read_file(&options.source, MAX_SOURCE)?)
        .map_err(|_| "source must be UTF-8")?;
    console_request(
        &options.socket,
        &json!({"source":source,"timeout_ms":options.timeout_ms}),
    )
}

#[cfg(windows)]
fn console_request(socket: &PathBuf, request: &Value) -> Result<Value, String> {
    let response =
        rove_client_core::console::send_request(socket, request.clone(), Duration::from_secs(8))?;
    validate_result(
        &serde_json::to_vec(&response).map_err(|e| e.to_string())?,
        true,
    )
}

#[cfg(unix)]
fn console_request(socket: &PathBuf, request: &Value) -> Result<Value, String> {
    let mut frame = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
    frame.push(b'\n');
    if frame.len() > 608 * 1024 * 1024 {
        return Err("source request exceeds 608 MiB".into());
    }
    let deadline = Instant::now() + Duration::from_secs(8);
    #[cfg(unix)]
    let mut stream = UnixStream::connect(socket)
        .map_err(|e| format!("cannot connect to client script console: {e}"))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(8)))
        .map_err(|e| e.to_string())?;
    stream.write_all(&frame).map_err(|e| e.to_string())?;
    let mut response = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("console response timed out".into());
        }
        stream
            .set_read_timeout(Some(remaining))
            .map_err(|e| e.to_string())?;
        let mut buffer = [0; 8192];
        let count = stream.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 {
            return Err("console closed without a newline-terminated result".into());
        }
        if response.len() + count > MAX_FRAME {
            return Err("console response exceeds 1 MiB".into());
        }
        response.extend_from_slice(&buffer[..count]);
        if let Some(end) = response.iter().position(|v| *v == b'\n') {
            if end + 1 != response.len() {
                return Err("console returned multiple frames".into());
            }
            return validate_result(&response[..end], true);
        }
    }
}

fn run(options: &Options, arguments: &[String]) -> Result<Value, String> {
    if options.submit {
        return submit(options);
    }
    if !options.worker {
        return supervise(options, arguments);
    }
    let source = read_file(&options.source, test_limits::source(MAX_SOURCE))?;
    std::str::from_utf8(&source).map_err(|_| "source must be UTF-8")?;
    let snapshot = options
        .snapshot
        .as_ref()
        .map(|path| read_file(path, MAX_SNAPSHOT))
        .transpose()?;
    let data = native::run(
        &source,
        &options.arguments,
        snapshot.as_deref(),
        options.timeout_ms,
        options.memory_mb,
    )?;
    validate_result(&data, true)
}

fn main() {
    let arguments: Vec<String> = env::args().skip(1).collect();
    if arguments.first().is_some_and(|v| v == "status") {
        let result = (|| -> Result<Value, String> {
            if arguments.len() != 2 && arguments.len() != 4 {
                return Err("usage: headless-luau status JOB [--socket PATH]".into());
            }
            let job: u64 = arguments[1]
                .parse()
                .map_err(|_| "job must be a positive integer")?;
            if job == 0 {
                return Err("job must be positive".into());
            }
            let socket = if arguments.len() == 4 {
                if arguments[2] != "--socket" {
                    return Err("expected --socket PATH".into());
                }
                PathBuf::from(&arguments[3])
            } else {
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .unwrap()
                    .join("secrets/client-script.sock")
            };
            console_request(&socket, &json!({"op":"status","job_id":job}))
        })();
        match result {
            Ok(value) => {
                println!("{value}");
                std::process::exit(if value["ok"] == true { 0 } else { 1 });
            }
            Err(error) => {
                println!("{}", json!({"ok":false,"values":[],"error":error}));
                std::process::exit(2);
            }
        }
    }
    if arguments == ["session"] || arguments == ["--session-worker"] {
        let result = if arguments[0] == "session" {
            session::supervise()
        } else {
            session::worker()
        };
        if let Err(error) = result {
            println!(
                "{}",
                json!({"ok":false,"values":[],"error":error,"state_lost":true})
            );
            std::process::exit(2);
        }
        return;
    }
    if arguments == ["--help"] || arguments == ["-h"] {
        println!("{USAGE}");
        return;
    }
    if arguments == ["--version"] {
        println!("headless-luau {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let (result, code) = match parse(&arguments).and_then(|options| run(&options, &arguments)) {
        Ok(result) => {
            let code = if result["ok"] == true { 0 } else { 1 };
            (result, code)
        }
        Err(error) => (json!({"ok":false,"values":[],"error":error}), 2),
    };
    println!("{result}");
    std::process::exit(code);
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }
    #[test]
    fn arguments_are_strict_and_bounded() {
        for input in [
            vec!["--source", "test", "--timeout-ms", "no"],
            vec!["--source", "test", "--memory-mb", "7"],
            vec!["--source", "test", "--", "NaN"],
            vec!["--source", "test", "--source", "other"],
            vec!["submit", "--script", "test", "--", "1"],
        ] {
            assert!(parse(&args(&input)).is_err());
        }
        let options = parse(&args(&["--source", "test", "--", "-1", "2.5"])).unwrap();
        assert_eq!(options.arguments, vec![-1.0, 2.5]);
    }
    #[test]
    fn result_protocol_checks_exit_and_shape() {
        assert!(validate_result(br#"{"ok":true,"values":[]}"#, true).is_ok());
        assert!(validate_result(br#"{"ok":true,"values":[]}"#, false).is_err());
        for bad in [
            b"null".as_slice(),
            br#"{"ok":1,"values":[]}"#,
            br#"{"ok":true}"#,
        ] {
            assert!(validate_result(bad, true).is_err());
        }
    }
}
