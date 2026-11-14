//! Localhost-only HTTP bridge for the shared Rove session UI.
//!
//! The server deliberately owns no game connection or VM. Requests are sent to
//! the single session owner over a bounded command channel, so opening another
//! browser tab cannot create a second client session.

use serde_json::{json, Map, Value};
use std::io::{self, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const HEADER_LIMIT: usize = 16 * 1024;
const SCRIPT_LIMIT: usize = 100 * 1024 * 1024;
const COMMAND_WAIT: Duration = Duration::from_secs(12);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiOperation {
    Status,
    RuntimeStatus,
    Render,
    Execute,
    Events,
    Gui,
    Movement,
    Keyboard,
    Display,
    UserChat,
    Jump,
    Leave,
    RenderAsset,
}

/// One request for the session owner. `payload` is validated by this module;
/// `reply` is a one-shot result channel. Keep the receiver on the owner thread
/// so the Luau VM and native session need not be `Send`.
#[derive(Debug)]
pub struct UiCommand {
    pub operation: UiOperation,
    pub payload: Value,
    pub reply: SyncSender<Result<Value, String>>,
}

#[derive(Clone)]
struct Owner {
    tx: SyncSender<UiCommand>,
}

impl Owner {
    fn call(
        &self,
        operation: UiOperation,
        payload: Value,
        wait: Duration,
    ) -> Result<Value, HttpError> {
        let (reply, rx) = mpsc::sync_channel(1);
        let command = UiCommand {
            operation,
            payload,
            reply,
        };
        match self.tx.try_send(command) {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                return Err(HttpError::unavailable("Local runtime queue is busy"))
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                return Err(HttpError::unavailable("Runtime disconnected"))
            }
        }
        match rx.recv_timeout(wait) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(message)) => Err(HttpError::bad_request(message)),
            Err(RecvTimeoutError::Timeout) => Err(HttpError::new(
                503,
                "The client is busy. Waiting for the next game frame…",
            )),
            Err(RecvTimeoutError::Disconnected) => {
                Err(HttpError::unavailable("Runtime disconnected"))
            }
        }
    }
}

pub struct UiConfig {
    /// Must be a loopback address. Non-loopback bind requests are rejected.
    pub bind: SocketAddr,
    pub assets: PathBuf,
    /// Random per-process key inserted into the existing UI page.
    pub key: String,
}

pub struct UiServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Vec<JoinHandle<()>>>>,
}

impl UiServer {
    pub fn bind(config: UiConfig, owner: SyncSender<UiCommand>) -> io::Result<Self> {
        if !config.bind.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "UI server must bind to loopback",
            ));
        }
        if config.key.len() < 32
            || !config
                .key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "UI key must be a URL-safe random token",
            ));
        }
        let listener = TcpListener::bind(config.bind)?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let owner = Owner { tx: owner };
        let assets = Arc::new(config.assets);
        let key = Arc::new(config.key);
        let (incoming_tx, incoming_rx) = mpsc::sync_channel::<TcpStream>(16);
        let incoming_rx = Arc::new(Mutex::new(incoming_rx));
        let thread = thread::Builder::new()
            .name("rove-ui-http".into())
            .spawn(move || {
                let mut workers = Vec::with_capacity(4);
                for worker_index in 0..4 {
                    let rx = Arc::clone(&incoming_rx);
                    let owner = owner.clone();
                    let assets = Arc::clone(&assets);
                    let key = Arc::clone(&key);
                    if let Ok(worker) = thread::Builder::new()
                        .name(format!("rove-ui-request-{worker_index}"))
                        .spawn(move || loop {
                            let stream = match rx.lock().ok().and_then(|guard| guard.recv().ok()) {
                                Some(stream) => stream,
                                None => break,
                            };
                            let _ = serve(stream, owner.clone(), &assets, &key, addr.port());
                        })
                    {
                        workers.push(worker);
                    }
                }
                while !worker_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, peer)) if peer.ip().is_loopback() => {
                            match incoming_tx.try_send(stream) {
                                Ok(()) => {}
                                Err(mpsc::TrySendError::Full(mut stream)) => {
                                    let _ = respond(
                                        &mut stream,
                                        503,
                                        json!({"ok":false,"error":"Local UI is busy"}),
                                        "application/json; charset=utf-8",
                                    );
                                }
                                Err(mpsc::TrySendError::Disconnected(_)) => break,
                            }
                        }
                        Ok((mut stream, _)) => {
                            let _ = respond(
                                &mut stream,
                                403,
                                json!({"ok":false,"error":"Invalid local host"}),
                                "application/json; charset=utf-8",
                            );
                        }
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10))
                        }
                        Err(_) => thread::sleep(Duration::from_millis(50)),
                    }
                }
                drop(incoming_tx);
                workers
            })?;
        Ok(Self {
            addr,
            stop,
            thread: Some(thread),
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(100));
        if let Some(thread) = self.thread.take() {
            if let Ok(workers) = thread.join() {
                for worker in workers {
                    let _ = worker.join();
                }
            }
        }
    }
}

impl Drop for UiServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Debug)]
struct HttpError {
    status: u16,
    message: String,
}
impl HttpError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(400, message)
    }
    fn unavailable(message: impl Into<String>) -> Self {
        Self::new(503, message)
    }
}

struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}
impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    fn path_query(&self) -> (&str, &str) {
        self.target
            .split_once('?')
            .map_or((self.target.as_str(), ""), |(p, q)| (p, q))
    }
}

fn serve(
    mut stream: TcpStream,
    owner: Owner,
    assets: &Path,
    key: &str,
    port: u16,
) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(15)))?;
    let request = match read_request(&mut stream) {
        Ok(r) => r,
        Err(e) => {
            let _ = respond(
                &mut stream,
                e.status,
                json!({"ok":false,"error":e.message}),
                "application/json; charset=utf-8",
            );
            return Ok(());
        }
    };
    let expected_hosts = [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ];
    if !request
        .header("host")
        .is_some_and(|h| expected_hosts.iter().any(|x| x.eq_ignore_ascii_case(h)))
    {
        return respond(
            &mut stream,
            403,
            json!({"ok":false,"error":"Invalid local host"}),
            "application/json; charset=utf-8",
        );
    }
    let (path, query) = request.path_query();
    if request.method == "GET" || request.method == "HEAD" {
        if path == "/api/status" {
            return json_response(
                &mut stream,
                owner.call(UiOperation::Status, Value::Null, COMMAND_WAIT),
            );
        }
        if path == "/api/runtime/status" {
            return json_response(
                &mut stream,
                owner.call(
                    UiOperation::RuntimeStatus,
                    Value::Null,
                    Duration::from_secs(5),
                ),
            );
        }
        if let Some((name, mime)) = static_asset_path(path) {
            return serve_asset(
                &mut stream,
                assets,
                name,
                mime,
                key,
                request.method == "HEAD",
            );
        }
        return respond(
            &mut stream,
            404,
            json!({"ok":false,"error":"Not found"}),
            "application/json; charset=utf-8",
        );
    }
    if request.method != "POST" {
        return respond(
            &mut stream,
            405,
            json!({"ok":false,"error":"Method not allowed"}),
            "application/json; charset=utf-8",
        );
    }
    let host = request.header("host").unwrap_or("");
    if request.header("origin") != Some(&format!("http://{host}"))
        || request.header("x-console-key") != Some(key)
    {
        return respond(
            &mut stream,
            403,
            json!({"ok":false,"error":"Reload this local console page and try again"}),
            "application/json; charset=utf-8",
        );
    }
    if request
        .header("content-type")
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        != "application/json"
    {
        return respond(
            &mut stream,
            415,
            json!({"ok":false,"error":"Expected JSON"}),
            "application/json; charset=utf-8",
        );
    }
    let data: Value = match serde_json::from_slice(&request.body) {
        Ok(v) => v,
        Err(_) => {
            return respond(
                &mut stream,
                400,
                json!({"ok":false,"error":"Invalid JSON request"}),
                "application/json; charset=utf-8",
            )
        }
    };
    let routed = route(path, query, data);
    match routed {
        Ok((operation, payload, wait)) => {
            json_response(&mut stream, owner.call(operation, payload, wait))
        }
        Err(e) => respond(
            &mut stream,
            e.status,
            json!({"ok":false,"error":e.message}),
            "application/json; charset=utf-8",
        ),
    }
}

fn read_request(stream: &mut TcpStream) -> Result<Request, HttpError> {
    let mut reader = BufReader::new(stream);
    let mut head = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if Instant::now() >= deadline {
            return Err(HttpError::new(408, "Request timed out"));
        }
        if head.len() >= HEADER_LIMIT {
            return Err(HttpError::new(431, "Request headers are too large"));
        }
        let mut byte = [0u8; 1];
        reader
            .read_exact(&mut byte)
            .map_err(|_| HttpError::bad_request("Incomplete request"))?;
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let text =
        std::str::from_utf8(&head).map_err(|_| HttpError::bad_request("Invalid HTTP headers"))?;
    let mut lines = text.split("\r\n");
    let first = lines
        .next()
        .ok_or_else(|| HttpError::bad_request("Invalid request line"))?;
    let mut words = first.split_whitespace();
    let method = words
        .next()
        .ok_or_else(|| HttpError::bad_request("Invalid request line"))?
        .to_owned();
    let target = words
        .next()
        .ok_or_else(|| HttpError::bad_request("Invalid request target"))?
        .to_owned();
    if words.next() != Some("HTTP/1.1") || !target.starts_with('/') {
        return Err(HttpError::bad_request("Unsupported HTTP request"));
    }
    let mut headers = Vec::new();
    let mut length = None;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| HttpError::bad_request("Invalid HTTP header"))?;
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err(HttpError::new(413, "Chunked requests are not supported"));
        }
        if name.eq_ignore_ascii_case("content-length") {
            length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| HttpError::bad_request("Invalid content length"))?,
            );
        }
        headers.push((name.trim().to_owned(), value.trim().to_owned()));
    }
    let length = length.unwrap_or(0);
    let request_path = target.split('?').next().unwrap_or(&target);
    let limit = match request_path {
        "/api/runtime/execute" => SCRIPT_LIMIT + 4096,
        "/api/runtime/input" => 16 * 1024,
        "/api/runtime/render-asset" => 2048,
        "/api/runtime/gui" => 16 * 1024,
        _ => 64 * 1024,
    };
    if length > limit {
        return Err(HttpError::new(413, "Invalid or oversized request"));
    }
    let mut body = vec![0u8; length];
    let mut offset = 0;
    while offset < body.len() {
        if Instant::now() >= deadline {
            return Err(HttpError::new(408, "Request timed out"));
        }
        let count = reader
            .read(&mut body[offset..])
            .map_err(|_| HttpError::bad_request("Incomplete request body"))?;
        if count == 0 {
            return Err(HttpError::bad_request("Incomplete request body"));
        }
        offset += count;
    }
    Ok(Request {
        method,
        target,
        headers,
        body,
    })
}

fn route(
    path: &str,
    _query: &str,
    data: Value,
) -> Result<(UiOperation, Value, Duration), HttpError> {
    let command = match path {
        "/api/runtime/render" => {
            if !data.is_object()
                || data.as_object().unwrap().keys().any(|k| k != "after")
                || data
                    .get("after")
                    .is_some_and(|v| !v.as_u64().is_some_and(|n| n > 0 && n < (1u64 << 53)))
            {
                return Err(HttpError::bad_request("Invalid render sample"));
            }
            (UiOperation::Render, data, Duration::from_secs(30))
        }
        "/api/runtime/execute" => {
            let obj = object(data, "Invalid script request")?;
            if obj.keys().any(|k| k != "source" && k != "timeout_ms") {
                return Err(HttpError::bad_request("Invalid script request"));
            }
            let source = obj
                .get("source")
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| HttpError::bad_request("Write a script first"))?;
            if source.len() > SCRIPT_LIMIT {
                return Err(HttpError::bad_request("Script exceeds 100 MiB"));
            }
            let budget = obj
                .get("timeout_ms")
                .map(|v| v.as_u64().filter(|n| (1..=2000).contains(n)))
                .unwrap_or(Some(1000))
                .ok_or_else(|| HttpError::bad_request("Script budget must be 1..2000 ms"))?;
            let wait = Duration::from_millis(budget + 2000);
            (UiOperation::Execute, Value::Object(obj), wait)
        }
        "/api/runtime/events" => {
            let obj = exact(data, &["after"], "Invalid runtime output cursor")?;
            if obj["after"].as_u64().is_none() {
                return Err(HttpError::bad_request("Invalid runtime output cursor"));
            }
            (UiOperation::Events, Value::Object(obj), COMMAND_WAIT)
        }
        "/api/runtime/gui" => {
            validate_gui(&data)?;
            (UiOperation::Gui, data, COMMAND_WAIT)
        }
        "/api/runtime/move" => {
            let obj = exact(
                data,
                &["client", "sequence", "direction"],
                "Invalid movement direction",
            )?;
            let vec = obj["direction"]
                .as_array()
                .filter(|v| v.len() == 3)
                .ok_or_else(|| HttpError::bad_request("Invalid movement direction"))?;
            if vec.iter().any(|v| !finite_bounded(v, 1.0)) || vec[1].as_f64() != Some(0.0) {
                return Err(HttpError::bad_request("Invalid movement direction"));
            }
            if obj["client"].as_str().is_none() || obj["sequence"].as_u64().is_none() {
                return Err(HttpError::bad_request("Invalid movement direction"));
            }
            (
                UiOperation::Movement,
                Value::Object(obj),
                Duration::from_secs(1),
            )
        }
        "/api/runtime/input" => {
            let obj = object(data, "Invalid keyboard request")?;
            if obj.get("action").and_then(Value::as_str).is_none() || obj.len() > 8 {
                return Err(HttpError::bad_request("Invalid keyboard request"));
            }
            (
                UiOperation::Keyboard,
                Value::Object(obj),
                Duration::from_secs(6),
            )
        }
        "/api/runtime/display" => {
            let obj = exact(data, &["size"], "Invalid display profile")?;
            let size = obj["size"]
                .as_array()
                .filter(|v| v.len() == 2)
                .ok_or_else(|| HttpError::bad_request("Invalid display profile"))?;
            if size
                .iter()
                .any(|v| v.as_u64().is_none_or(|n| !(1..=8192).contains(&n)))
            {
                return Err(HttpError::bad_request("Invalid display profile"));
            }
            (UiOperation::Display, Value::Object(obj), COMMAND_WAIT)
        }
        "/api/runtime/user-chat" => {
            let obj = object(data, "Invalid typed chat submission")?;
            if obj.keys().any(|k| k != "text" && k != "channel")
                || obj.get("text").and_then(Value::as_str).is_none()
                || obj
                    .get("channel")
                    .is_some_and(|v| !matches!(v.as_str(), Some("RBXGeneral" | "RBXGlobal")))
            {
                return Err(HttpError::bad_request("Invalid typed chat submission"));
            }
            (UiOperation::UserChat, Value::Object(obj), COMMAND_WAIT)
        }
        "/api/runtime/jump" => {
            if data != json!({"action":"jump"}) {
                return Err(HttpError::bad_request("Invalid jump request"));
            }
            (UiOperation::Jump, data, COMMAND_WAIT)
        }
        "/api/runtime/leave" => {
            if data != json!({}) {
                return Err(HttpError::bad_request("Unexpected session request fields"));
            }
            (UiOperation::Leave, data, COMMAND_WAIT)
        }
        "/api/runtime/render-asset" => {
            if serde_json::to_vec(&data).map_or(usize::MAX, |x| x.len()) > 2048 {
                return Err(HttpError::new(413, "Visual asset request exceeds limit"));
            }
            let obj = object(data, "Invalid visual asset request")?;
            let is_font = obj.get("kind").and_then(Value::as_str) == Some("font");
            if (is_font
                && obj
                    .keys()
                    .any(|k| !["kind", "reference", "weight", "style"].contains(&k.as_str())))
                || (!is_font
                    && (obj.len() != 2 || obj.keys().any(|k| k != "kind" && k != "reference")))
                || obj.get("reference").and_then(Value::as_str).is_none()
            {
                return Err(HttpError::bad_request("Invalid visual asset request"));
            }
            if is_font
                && (obj.get("weight").and_then(Value::as_u64).is_none()
                    || obj.get("style").and_then(Value::as_u64).is_none())
            {
                return Err(HttpError::bad_request("Invalid visual asset request"));
            }
            (
                UiOperation::RenderAsset,
                Value::Object(obj),
                Duration::from_secs(20),
            )
        }
        _ => return Err(HttpError::new(404, "Not found")),
    };
    Ok(command)
}

fn object(value: Value, error: &str) -> Result<Map<String, Value>, HttpError> {
    value
        .as_object()
        .cloned()
        .ok_or_else(|| HttpError::bad_request(error))
}
fn exact(value: Value, fields: &[&str], error: &str) -> Result<Map<String, Value>, HttpError> {
    let obj = object(value, error)?;
    if obj.len() != fields.len() || obj.keys().any(|k| !fields.contains(&k.as_str())) {
        return Err(HttpError::bad_request(error));
    }
    Ok(obj)
}
fn finite_bounded(value: &Value, limit: f64) -> bool {
    value
        .as_f64()
        .is_some_and(|n| n.is_finite() && n.abs() <= limit)
}

fn validate_gui(value: &Value) -> Result<(), HttpError> {
    let obj = value
        .as_object()
        .ok_or_else(|| HttpError::bad_request("Invalid GUI action"))?;
    let kind = obj
        .get("kind")
        .and_then(Value::as_str)
        .ok_or_else(|| HttpError::bad_request("Invalid GUI action"))?;
    let mut fields: Vec<&str> = match kind {
        "text" => vec!["id", "kind", "text"],
        "image_loaded" => vec!["id", "kind", "token"],
        "text_edit" => vec!["id", "kind", "revision", "text"],
        "text_select" => vec!["id", "kind", "revision"],
        "text_blur" => vec!["id", "kind", "revision", "submitted"],
        "wheel" => vec!["id", "kind", "delta", "position", "steps"],
        "scroll" => vec!["id", "kind", "delta"],
        "click" | "text_focus" | "pointer_down" | "pointer_up" => vec!["id", "kind"],
        _ => return Err(HttpError::bad_request("Invalid GUI action")),
    };
    let selection =
        kind == "text_select" || (kind == "text_edit" && obj.contains_key("editorRevision"));
    if selection {
        fields.extend(["editorRevision", "cursor", "anchor"]);
    }
    if obj.len() != fields.len()
        || obj.keys().any(|k| !fields.contains(&k.as_str()))
        || obj
            .get("id")
            .and_then(Value::as_str)
            .is_none_or(|s| s.len() > 128)
    {
        return Err(HttpError::bad_request("Invalid GUI action"));
    }
    if matches!(kind, "text" | "text_edit")
        && obj
            .get("text")
            .and_then(Value::as_str)
            .is_none_or(|s| s.len() > 4096)
    {
        return Err(HttpError::bad_request("Invalid GUI action"));
    }
    if kind == "image_loaded"
        && obj.get("token").and_then(Value::as_str).is_none_or(|s| {
            s.len() != 32
                || !s
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        })
    {
        return Err(HttpError::bad_request("Invalid image load receipt"));
    }
    if matches!(kind, "text_edit" | "text_select" | "text_blur")
        && obj.get("revision").and_then(Value::as_u64).is_none()
    {
        return Err(HttpError::bad_request("Invalid TextBox interaction"));
    }
    if kind == "text_blur" && obj.get("submitted").and_then(Value::as_bool).is_none() {
        return Err(HttpError::bad_request("Invalid TextBox interaction"));
    }
    if selection {
        if obj.get("editorRevision").and_then(Value::as_u64).is_none()
            || obj
                .get("cursor")
                .and_then(Value::as_u64)
                .is_none_or(|x| !(1..=4097).contains(&x))
            || obj
                .get("anchor")
                .and_then(Value::as_i64)
                .is_none_or(|x| !(-1..=4097).contains(&x) || x == 0)
        {
            return Err(HttpError::bad_request("Invalid TextBox selection"));
        }
    }
    if matches!(kind, "scroll" | "wheel") && !finite_pair(obj.get("delta"), 4096.0) {
        return Err(HttpError::bad_request("Invalid GUI scroll delta"));
    }
    if kind == "wheel"
        && (!finite_pair(obj.get("position"), 32768.0)
            || obj
                .get("steps")
                .and_then(Value::as_i64)
                .is_none_or(|n| !(-128..=128).contains(&n)))
    {
        return Err(HttpError::bad_request("Invalid GUI wheel input"));
    }
    Ok(())
}
fn finite_pair(value: Option<&Value>, limit: f64) -> bool {
    value
        .and_then(Value::as_array)
        .is_some_and(|a| a.len() == 2 && a.iter().all(|v| finite_bounded(v, limit)))
}

fn json_response(stream: &mut TcpStream, value: Result<Value, HttpError>) -> io::Result<()> {
    match value {
        Ok(body) => respond(stream, 200, body, "application/json; charset=utf-8"),
        Err(e) => respond(
            stream,
            e.status,
            json!({"ok":false,"error":e.message}),
            "application/json; charset=utf-8",
        ),
    }
}
fn respond(stream: &mut TcpStream, status: u16, body: Value, mime: &str) -> io::Result<()> {
    let bytes = serde_json::to_vec(&body)
        .unwrap_or_else(|_| b"{\"ok\":false,\"error\":\"serialization failed\"}".to_vec());
    response_bytes(stream, status, &bytes, mime, false)
}
fn response_bytes(
    stream: &mut TcpStream,
    status: u16,
    body: &[u8],
    mime: &str,
    head: bool,
) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        408 => "Request Timeout",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        415 => "Unsupported Media Type",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    };
    write!(stream, "HTTP/1.1 {status} {reason}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nCross-Origin-Resource-Policy: same-origin\r\nContent-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' https://*.rbxcdn.com; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'\r\nConnection: close\r\n\r\n", body.len())?;
    if !head {
        stream.write_all(body)?;
    }
    let _ = stream.shutdown(Shutdown::Write);
    Ok(())
}

fn static_asset_path(path: &str) -> Option<(&str, &'static str)> {
    let fixed: &[(&str, &str, &str)] = &[
        ("/session", "local-session.html", "text/html; charset=utf-8"),
        ("/", "index.html", "text/html; charset=utf-8"),
        ("/workspace", "index.html", "text/html; charset=utf-8"),
        ("/workspace/", "index.html", "text/html; charset=utf-8"),
        ("/app.js", "app.js", "text/javascript; charset=utf-8"),
        (
            "/workspace.js",
            "workspace.js",
            "text/javascript; charset=utf-8",
        ),
        ("/input.js", "input.js", "text/javascript; charset=utf-8"),
        ("/style.css", "style.css", "text/css; charset=utf-8"),
        (
            "/keyboard-codes.json",
            "keyboard-codes.json",
            "application/json",
        ),
        (
            "/assets/navigation.svg",
            "assets/navigation.svg",
            "image/svg+xml",
        ),
        ("/assets/common.svg", "assets/common.svg", "image/svg+xml"),
        ("/assets/games.svg", "assets/games.svg", "image/svg+xml"),
        ("/assets/actions.svg", "assets/actions.svg", "image/svg+xml"),
        (
            "/assets/wordmark.svg",
            "assets/wordmark.svg",
            "image/svg+xml",
        ),
        ("/assets/chevron.svg", "assets/chevron.svg", "image/svg+xml"),
    ];
    fixed
        .iter()
        .find(|(url, _, _)| *url == path)
        .map(|(_, file, mime)| (*file, *mime))
        .or_else(|| {
            let name = path.strip_prefix('/')?;
            if name.is_empty()
                || name.contains('/')
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
            {
                return None;
            }
            let ext = name.rsplit_once('.')?.1;
            match ext {
                "js" => Some((name, "text/javascript; charset=utf-8")),
                "css" => Some((name, "text/css; charset=utf-8")),
                _ => None,
            }
        })
}
fn serve_asset(
    stream: &mut TcpStream,
    root: &Path,
    relative: &str,
    mime: &str,
    key: &str,
    head: bool,
) -> io::Result<()> {
    let path = root.join(relative);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => meta,
        _ => {
            return respond(
                stream,
                404,
                json!({"error":"UI file not found. Restart Rove after updating."}),
                "application/json; charset=utf-8",
            )
        }
    };
    if metadata.len() > 16 * 1024 * 1024 {
        return respond(
            stream,
            413,
            json!({"error":"UI asset exceeds size limit"}),
            "application/json; charset=utf-8",
        );
    }
    let mut bytes = match std::fs::read(path) {
        Ok(v) => v,
        Err(_) => {
            return respond(
                stream,
                404,
                json!({"error":"UI file not found."}),
                "application/json; charset=utf-8",
            )
        }
    };
    if matches!(relative, "index.html" | "local-session.html") {
        let key_meta = format!("<meta name=\"console-key\" content=\"{key}\">");
        let launcher_meta = format!("<meta name=\"rove-key\" content=\"{key}\">");
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        if text.contains("__LAUNCHER_KEY__") {
            text = text.replace("__LAUNCHER_KEY__", key);
        } else if text.contains("__CONSOLE_KEY__") {
            text = text.replace("__CONSOLE_KEY__", key);
        } else if !text.contains("name=\"console-key\"") {
            text = text.replacen("<head>", &format!("<head>{key_meta}{launcher_meta}"), 1);
        }
        bytes = text.into_bytes();
    }
    response_bytes(stream, 200, &bytes, mime, head)
}

/// Make a token from the operating system entropy source; callers can persist
/// it only for the lifetime of the local UI process.
pub fn random_ui_key() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    rove_client_core::entropy::fill(&mut bytes)?;
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-";
    Ok(bytes
        .iter()
        .map(|b| ALPHABET[*b as usize % ALPHABET.len()] as char)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;
    use std::sync::mpsc;

    fn request(
        addr: SocketAddr,
        method: &str,
        path: &str,
        data: Option<Value>,
        key: &str,
    ) -> (u16, Value) {
        let (status, body) = request_raw(addr, method, path, data, key);
        (status, serde_json::from_str(&body).unwrap_or(Value::Null))
    }

    fn request_raw(
        addr: SocketAddr,
        method: &str,
        path: &str,
        data: Option<Value>,
        key: &str,
    ) -> (u16, String) {
        let mut stream = TcpStream::connect(addr).unwrap();
        let body = data
            .map(|v| serde_json::to_vec(&v).unwrap())
            .unwrap_or_default();
        let host = format!("127.0.0.1:{}", addr.port());
        let origin = format!("http://{host}");
        write!(stream, "{method} {path} HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nX-Console-Key: {key}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
        stream.write_all(&body).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let (head, body) = response.split_once("\r\n\r\n").unwrap();
        let status = head
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        (status, body.to_owned())
    }

    fn mock_owner(rx: Receiver<UiCommand>) -> JoinHandle<()> {
        thread::spawn(move || {
            while let Ok(cmd) = rx.recv() {
                let response = match cmd.operation {
                    UiOperation::RuntimeStatus => json!({"connected":true,"session":"mock"}),
                    UiOperation::Render => json!({"ok":true,"frame":{"parts":[]}}),
                    UiOperation::Leave => json!({"ok":true}),
                    _ => json!({"ok":true,"values":[]}),
                };
                let _ = cmd.reply.send(Ok(response));
            }
        })
    }

    #[test]
    fn loopback_session_routes_use_owner_and_validate() {
        let (tx, rx) = mpsc::sync_channel(8);
        let owner = mock_owner(rx);
        let assets = std::env::temp_dir().join(format!("rove-ui-assets-{}", std::process::id()));
        std::fs::create_dir_all(&assets).unwrap();
        std::fs::write(
            assets.join("index.html"),
            "<html><head></head><body>console</body></html>",
        )
        .unwrap();
        std::fs::write(
            assets.join("local-session.html"),
            "<html><head><meta name=\"console-key\" content=\"__LAUNCHER_KEY__\"></head><body>local session</body></html>",
        )
        .unwrap();
        let mut server = UiServer::bind(
            UiConfig {
                bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
                assets: assets.clone(),
                key: "a".repeat(32),
            },
            tx,
        )
        .unwrap();
        let key = "a".repeat(32);
        let (status, page) = request_raw(server.local_addr(), "GET", "/workspace", None, &key);
        assert_eq!(status, 200);
        assert!(page.contains(&key));
        assert!(!page.contains("__LAUNCHER_KEY__"));
        let (status, page) = request_raw(server.local_addr(), "GET", "/session", None, &key);
        assert_eq!(status, 200);
        assert!(page.contains(&key));
        assert!(page.contains("local session"));
        let (status, value) = request(
            server.local_addr(),
            "GET",
            "/api/runtime/status",
            None,
            &key,
        );
        assert_eq!(status, 200);
        assert_eq!(value["connected"], true);
        let (status, value) = request(
            server.local_addr(),
            "POST",
            "/api/runtime/render",
            Some(json!({})),
            &key,
        );
        assert_eq!(status, 200);
        assert_eq!(value["frame"]["parts"], json!([]));
        let (status, _) = request(
            server.local_addr(),
            "POST",
            "/api/runtime/gui",
            Some(json!({"id":"x","kind":"scroll","delta":[1,2],"source":"bad"})),
            &key,
        );
        assert_eq!(status, 400);
        let (status, value) = request(
            server.local_addr(),
            "POST",
            "/api/runtime/leave",
            Some(json!({})),
            &key,
        );
        assert_eq!(status, 200);
        assert_eq!(value["ok"], true);
        server.shutdown();
        drop(owner);
        let _ = std::fs::remove_dir_all(assets);
    }

    #[test]
    fn route_schema_rejects_invalid_payloads_without_owner_calls() {
        for (path, value) in [
            ("/api/runtime/render", json!({"after":true})),
            (
                "/api/runtime/move",
                json!({"client":"x","sequence":1,"direction":[0,1,0]}),
            ),
            (
                "/api/runtime/gui",
                json!({"id":"x","kind":"wheel","delta":[0,0],"position":[0,0],"steps":129}),
            ),
            (
                "/api/runtime/execute",
                json!({"source":"print(1)","timeout_ms":9999}),
            ),
        ] {
            assert!(route(path, "", value).is_err(), "{path}");
        }
        assert!(route("/api/runtime/execute", "", json!({"source":"return 42"})).is_ok());
    }

    #[test]
    fn full_owner_queue_is_rejected_without_blocking_and_disconnect_is_bounded() {
        let (tx, rx) = mpsc::sync_channel(1);
        let owner = Owner { tx: tx.clone() };
        tx.try_send(UiCommand {
            operation: UiOperation::Status,
            payload: Value::Null,
            reply: mpsc::sync_channel(1).0,
        })
        .unwrap();
        let started = std::time::Instant::now();
        let error = owner
            .call(UiOperation::Render, json!({}), COMMAND_WAIT)
            .unwrap_err();
        assert_eq!(error.status, 503);
        assert!(started.elapsed() < Duration::from_millis(100));
        drop(rx);
        assert_eq!(
            owner
                .call(UiOperation::Status, Value::Null, COMMAND_WAIT)
                .unwrap_err()
                .status,
            503
        );
    }
}
