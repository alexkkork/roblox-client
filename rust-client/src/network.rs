//! Explicit script HTTP/WebSocket calls. No ambient Roblox authentication.
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::{self, ErrorKind, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock, TryLockError},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tungstenite::{stream::MaybeTlsStream, Message, WebSocket};

type Socket = WebSocket<MaybeTlsStream<DeadlineStream>>;

/// Adds an absolute deadline to each blocking socket operation. The deadline
/// is shared by TCP/TLS/WebSocket redirect attempts. DNS resolution itself
/// uses the platform's blocking resolver and cannot be interrupted here.
#[derive(Debug)]
struct DeadlineStream {
    stream: TcpStream,
    deadline: Option<Instant>,
}

impl DeadlineStream {
    fn remaining(&self) -> io::Result<Option<Duration>> {
        let Some(deadline) = self.deadline else {
            return Ok(None);
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                ErrorKind::TimedOut,
                "WebSocket I/O deadline exceeded",
            ));
        }
        Ok(Some(remaining))
    }

    fn set_post_handshake_timeouts(&mut self) -> io::Result<()> {
        self.deadline = None;
        self.stream
            .set_read_timeout(Some(Duration::from_millis(1)))?;
        self.stream
            .set_write_timeout(Some(Duration::from_millis(200)))
    }
}

impl Read for DeadlineStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if let Some(remaining) = self.remaining()? {
            self.stream.set_read_timeout(Some(remaining))?;
        }
        self.stream.read(buffer)
    }
}

impl Write for DeadlineStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if let Some(remaining) = self.remaining()? {
            self.stream.set_write_timeout(Some(remaining))?;
        }
        self.stream.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(remaining) = self.remaining()? {
            self.stream.set_write_timeout(Some(remaining))?;
        }
        self.stream.flush()
    }
}
// USSI downloads versioned API dumps through HttpService. Keep a clear upper
// bound, but allow the multi-megabyte documents its serializer needs.
const HTTP_RESPONSE_LIMIT: usize = 16 * 1024 * 1024;
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const WEBSOCKET_CONNECT_TIMEOUT: Duration = HTTP_REQUEST_TIMEOUT;
const WEBSOCKET_POLL_BUDGET: Duration = Duration::from_millis(2);
const HTTP_TRACE_LIMIT: u64 = 1024 * 1024;
const HTTP_TRACE_LINE_LIMIT: usize = 4096;
const HTTP_TRACE_URLS_LIMIT: usize = 64;

struct HttpResponseTrace {
    path: PathBuf,
    allowed_urls: HashSet<String>,
}

fn trace_url_is_safe(raw: &str) -> bool {
    if raw.len() > 2048 || raw.contains(['\r', '\n']) {
        return false;
    }
    let Ok(url) = url::Url::parse(raw) else {
        return false;
    };
    matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
}

fn response_trace_config() -> Option<HttpResponseTrace> {
    let path = std::env::var_os("RCR_HTTP_RESPONSE_TRACE")?;
    let raw_urls = std::env::var("RCR_HTTP_RESPONSE_TRACE_URLS").ok()?;
    if raw_urls.len() > 16 * 1024 {
        return None;
    }
    let urls: Vec<String> = serde_json::from_str(&raw_urls).ok()?;
    if urls.is_empty()
        || urls.len() > HTTP_TRACE_URLS_LIMIT
        || !urls.iter().all(|u| trace_url_is_safe(u))
    {
        return None;
    }
    Some(HttpResponseTrace {
        path: PathBuf::from(path),
        allowed_urls: urls.into_iter().collect(),
    })
}

fn response_trace_record(
    method: &str,
    requested_url: &str,
    effective_url: &str,
    status: u16,
    content_type: Option<&str>,
    body: &[u8],
    elapsed_ms: u128,
    allowed_urls: &HashSet<String>,
) -> Option<Value> {
    if method != "GET"
        || !allowed_urls.contains(requested_url)
        || !allowed_urls.contains(effective_url)
        || !trace_url_is_safe(requested_url)
        || !trace_url_is_safe(effective_url)
    {
        return None;
    }
    let digest = Sha256::digest(body);
    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    Some(json!({
        "requested_url": requested_url,
        "effective_url": effective_url,
        "unix_ms": unix_ms,
        "status": status,
        "content_type": content_type,
        "body_length": body.len(),
        "body_sha256": format!("{digest:x}"),
        "elapsed_ms": elapsed_ms,
    }))
}

fn append_http_trace(path: &Path, record: &Value) {
    // Tracing is best-effort and must never affect the HTTP response. Keep
    // both each record and the full file bounded; reject symlinks/non-files.
    let Ok(mut line) = serde_json::to_vec(record) else {
        return;
    };
    line.push(b'\n');
    if line.len() > HTTP_TRACE_LINE_LIMIT {
        return;
    }
    static TRACE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let Ok(_guard) = TRACE_LOCK.get_or_init(|| Mutex::new(())).lock() else {
        return;
    };
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => return,
        Err(error) if error.kind() != ErrorKind::NotFound => return,
        _ => {}
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
        let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)
        else {
            return;
        };
        let Ok(metadata) = file.metadata() else {
            return;
        };
        let Ok(path_metadata) = fs::symlink_metadata(path) else {
            return;
        };
        if !metadata.is_file()
            || !path_metadata.file_type().is_file()
            || metadata.dev() != path_metadata.dev()
            || metadata.ino() != path_metadata.ino()
            || metadata.nlink() != 1
        {
            return;
        }
        if file
            .set_permissions(fs::Permissions::from_mode(0o600))
            .is_err()
        {
            return;
        }
        if metadata.len().saturating_add(line.len() as u64) > HTTP_TRACE_LIMIT {
            return;
        }
        if file.write_all(&line).is_ok() {
            let _ = file.flush();
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, line);
    }
}
#[derive(Default)]
struct Sockets {
    next: u64,
    sockets: HashMap<u64, Socket>,
}
fn sockets() -> &'static Mutex<Sockets> {
    static S: OnceLock<Mutex<Sockets>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(Sockets::default()))
}
fn remove_socket(id: u64) {
    let removed = match sockets().lock() {
        Ok(mut pool) => pool.sockets.remove(&id),
        Err(poisoned) => poisoned.into_inner().sockets.remove(&id),
    };
    // Dropping a just-created, unclaimed socket closes its stream. Do so after
    // releasing the pool lock: cancellation cleanup must not block other I/O.
    drop(removed);
}
#[derive(Default)]
struct Jobs {
    next: u64,
    pending: HashMap<u64, NetworkJob>,
}
struct NetworkJob {
    operation: String,
    result: Option<Value>,
    discarded: bool,
}
impl Jobs {
    fn publish(&mut self, id: u64, result: Value) -> Option<u64> {
        if let Some(slot) = self.pending.get_mut(&id) {
            if slot.discarded {
                let discard_socket = (slot.operation == "connect")
                    .then(|| result["id"].as_u64())
                    .flatten();
                self.pending.remove(&id);
                return discard_socket;
            } else {
                slot.result = Some(result);
            }
        }
        None
    }

    fn discard_http(&mut self, id: u64) -> Result<(), String> {
        let Some(slot) = self.pending.get_mut(&id) else {
            return Ok(());
        };
        if slot.operation != "http" {
            return Err("cannot discard a non-HTTP network job".into());
        }
        // A canceled Lua waiter does not cancel a blocking socket operation.
        // Keep its slot charged against the pool until the worker finishes,
        // otherwise repeated cancellation could start unbounded OS threads.
        if slot.result.is_some() {
            self.pending.remove(&id);
        } else {
            slot.discarded = true;
        }
        Ok(())
    }

    fn discard_network_job(&mut self, id: u64) -> Result<Option<u64>, String> {
        let Some(slot) = self.pending.get_mut(&id) else {
            return Ok(None);
        };
        if !["http", "connect", "send", "close"].contains(&slot.operation.as_str()) {
            return Err("cannot discard this network job".into());
        }
        if slot.result.is_some() {
            let socket_id = (slot.operation == "connect")
                .then(|| {
                    slot.result
                        .as_ref()
                        .and_then(|result| result["id"].as_u64())
                })
                .flatten();
            self.pending.remove(&id);
            Ok(socket_id)
        } else {
            slot.discarded = true;
            Ok(None)
        }
    }

    fn claim_network_job(&mut self, id: u64) -> Result<(), String> {
        let Some(slot) = self.pending.get(&id) else {
            return Ok(());
        };
        if !["http", "connect", "send", "close"].contains(&slot.operation.as_str()) {
            return Err("cannot claim this network job".into());
        }
        if slot.discarded || slot.result.is_none() {
            return Err("network job is not ready to claim".into());
        }
        self.pending.remove(&id);
        Ok(())
    }
}
fn jobs() -> &'static Mutex<Jobs> {
    static J: OnceLock<Mutex<Jobs>> = OnceLock::new();
    J.get_or_init(|| Mutex::new(Jobs::default()))
}
fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str, String> {
    v[key].as_str().ok_or_else(|| format!("{key} must be text"))
}
fn timeout(stream: &mut MaybeTlsStream<DeadlineStream>) -> Result<(), String> {
    let socket = match stream {
        MaybeTlsStream::Plain(s) => s,
        MaybeTlsStream::NativeTls(s) => s.get_mut(),
        _ => return Err("unsupported TLS stream".into()),
    };
    socket
        .set_post_handshake_timeouts()
        .map_err(|e| e.to_string())
}
fn set_poll_deadline(
    stream: &mut MaybeTlsStream<DeadlineStream>,
    deadline: Instant,
) -> Result<(), String> {
    let stream = match stream {
        MaybeTlsStream::Plain(stream) => stream,
        MaybeTlsStream::NativeTls(stream) => stream.get_mut(),
        _ => return Err("unsupported TLS stream".into()),
    };
    // Read/Write apply the remaining budget immediately before the syscall.
    // An expired poll is a transient timeout, not a setup failure that would
    // make the receiver abandon an otherwise healthy socket.
    stream.deadline = Some(deadline);
    Ok(())
}
fn websocket_url(raw: &str, base: Option<&url::Url>) -> Result<url::Url, String> {
    let mut url = match base {
        Some(base) => base.join(raw),
        None => url::Url::parse(raw),
    }
    .map_err(|e| e.to_string())?;
    let scheme = match url.scheme() {
        "ws" | "http" => "ws",
        "wss" | "https" => "wss",
        _ => return Err("WS(S) URL required".into()),
    };
    url.set_scheme(scheme)
        .map_err(|_| "Invalid WebSocket URL")?;
    if url.host_str().is_none()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(
            "WebSocket URL requires a host and cannot contain credentials or a fragment".into(),
        );
    }
    Ok(url)
}
fn connect_socket(raw: &str) -> Result<Socket, String> {
    connect_socket_with_timeout(raw, WEBSOCKET_CONNECT_TIMEOUT)
}

fn connect_socket_with_timeout(raw: &str, deadline_budget: Duration) -> Result<Socket, String> {
    let mut url = websocket_url(raw, None)?;
    let deadline = Instant::now() + deadline_budget;
    let config = tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(128 * 1024))
        .max_frame_size(Some(128 * 1024));
    for attempt in 0..=3 {
        if Instant::now() >= deadline {
            return Err("WebSocket connect deadline exceeded".into());
        }
        // Rebuild the handshake for each destination so Host and TLS SNI
        // follow the redirected URL. No Roblox or previous-origin headers
        // are attached to script WebSockets.
        let port = url
            .port_or_known_default()
            .ok_or("WebSocket URL requires a port")?;
        // Name resolution is synchronous in std::net::ToSocketAddrs and can
        // exceed the connection deadline. All resolved-address connection,
        // TLS, and WebSocket handshake I/O below shares the absolute deadline.
        let addresses = match url.host().ok_or("WebSocket URL requires a host")? {
            url::Host::Ipv4(address) => vec![std::net::SocketAddr::new(
                std::net::IpAddr::V4(address),
                port,
            )],
            url::Host::Ipv6(address) => vec![std::net::SocketAddr::new(
                std::net::IpAddr::V6(address),
                port,
            )],
            url::Host::Domain(host) => (host, port)
                .to_socket_addrs()
                .map_err(|e| e.to_string())?
                .collect::<Vec<_>>(),
        };
        let mut connected = None;
        let mut last_connect_error = None;
        for address in addresses {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match TcpStream::connect_timeout(&address, remaining) {
                Ok(stream) => {
                    connected = Some(stream);
                    break;
                }
                Err(error) => last_connect_error = Some(error),
            }
        }
        let stream = match connected {
            Some(stream) => stream,
            None if Instant::now() >= deadline => {
                return Err("WebSocket connect deadline exceeded".into());
            }
            None => {
                return Err(last_connect_error
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "WebSocket URL resolved to no addresses".into()))
            }
        };
        stream.set_nodelay(true).map_err(|e| e.to_string())?;
        let stream = DeadlineStream {
            stream,
            deadline: Some(deadline),
        };
        let request = tungstenite::client::IntoClientRequest::into_client_request(url.as_str())
            .map_err(|e| e.to_string())?;
        let result = match url.scheme() {
            "ws" => tungstenite::client::client_with_config(
                request,
                MaybeTlsStream::Plain(stream),
                Some(config),
            )
            .map_err(|error| match error {
                tungstenite::handshake::HandshakeError::Failure(error) => error,
                tungstenite::handshake::HandshakeError::Interrupted(_) => tungstenite::Error::Io(
                    io::Error::new(ErrorKind::TimedOut, "WebSocket handshake interrupted"),
                ),
            }),
            "wss" => tungstenite::client_tls_with_config(request, stream, Some(config), None)
                .map_err(|error| match error {
                    tungstenite::handshake::HandshakeError::Failure(error) => error,
                    tungstenite::handshake::HandshakeError::Interrupted(_) => {
                        tungstenite::Error::Io(io::Error::new(
                            ErrorKind::TimedOut,
                            "WebSocket handshake interrupted",
                        ))
                    }
                }),
            _ => return Err("WS(S) URL required".into()),
        };
        match result {
            Ok((mut socket, _)) => {
                timeout(socket.get_mut())?;
                return Ok(socket);
            }
            Err(tungstenite::Error::Http(response))
                if response.status().is_redirection() && attempt < 3 =>
            {
                let location = response
                    .headers()
                    .get("Location")
                    .ok_or("WebSocket redirect is missing Location")?
                    .to_str()
                    .map_err(|e| e.to_string())?;
                let next = websocket_url(location, Some(&url))?;
                if url.scheme() == "wss" && next.scheme() != "wss" {
                    return Err("WebSocket redirect would downgrade TLS".into());
                }
                url = next;
            }
            Err(tungstenite::Error::Http(response)) if response.status().is_redirection() => {
                return Err("WebSocket redirect limit exceeded".into());
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    unreachable!()
}
pub(crate) fn operation(v: Value) -> Result<Value, String> {
    operation_with_ws_timeout(v, WEBSOCKET_CONNECT_TIMEOUT)
}

fn operation_with_ws_timeout(v: Value, websocket_timeout: Duration) -> Result<Value, String> {
    match text(&v, "operation")? {
        "start" => {
            let request = v["request"].clone();
            let request_operation = text(&request, "operation")?.to_owned();
            if !["http", "connect", "send", "close", "decompile"]
                .contains(&request_operation.as_str())
            {
                return Err("invalid asynchronous operation".into());
            }
            let mut jobs = self::jobs().lock().map_err(|_| "job pool lock failed")?;
            if jobs.pending.len() >= 16 {
                return Err("network job limit exceeded".into());
            }
            jobs.next += 1;
            let id = jobs.next;
            jobs.pending.insert(
                id,
                NetworkJob {
                    operation: request_operation,
                    result: None,
                    discarded: false,
                },
            );
            drop(jobs);
            thread::spawn(move || {
                let result = match operation(request) {
                    Ok(v) => v,
                    Err(e) => json!({"error":e}),
                };
                let discard_socket = if let Ok(mut jobs) = self::jobs().lock() {
                    jobs.publish(id, result)
                } else {
                    None
                };
                if let Some(socket_id) = discard_socket {
                    remove_socket(socket_id);
                }
            });
            Ok(json!({"id":id}))
        }
        "result" => {
            let id = v["id"].as_u64().ok_or("job ID required")?;
            let mut jobs = self::jobs().lock().map_err(|_| "job pool lock failed")?;
            let slot = jobs.pending.get_mut(&id).ok_or("unknown network job")?;
            if slot.discarded {
                return Err("network job was discarded".into());
            }
            let retain_until_claim = slot.operation == "connect";
            let value = if retain_until_claim {
                slot.result.as_ref().cloned()
            } else {
                slot.result.take()
            };
            if let Some(value) = value {
                if !retain_until_claim {
                    jobs.pending.remove(&id);
                }
                Ok(json!({"ready":true,"value":value}))
            } else {
                Ok(json!({"ready":false}))
            }
        }
        "discard_http" => {
            let id = v["id"].as_u64().ok_or("job ID required")?;
            self::jobs()
                .lock()
                .map_err(|_| "job pool lock failed")?
                .discard_http(id)?;
            Ok(json!({}))
        }
        "discard_network_job" => {
            let id = v["id"].as_u64().ok_or("job ID required")?;
            let socket_id = self::jobs()
                .lock()
                .map_err(|_| "job pool lock failed")?
                .discard_network_job(id)?;
            if let Some(socket_id) = socket_id {
                remove_socket(socket_id);
            }
            Ok(json!({}))
        }
        "claim_network_job" => {
            let id = v["id"].as_u64().ok_or("job ID required")?;
            self::jobs()
                .lock()
                .map_err(|_| "job pool lock failed")?
                .claim_network_job(id)?;
            Ok(json!({}))
        }
        "decompile" => crate::decompiler::decompile(&v),
        "http" => {
            let url = text(&v, "Url")?;
            if !(url.starts_with("https://") || url.starts_with("http://")) {
                return Err("HTTP(S) URL required".into());
            }
            let method = v["Method"].as_str().unwrap_or("GET");
            if !["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"].contains(&method) {
                return Err("unsupported HTTP method".into());
            }
            let agent = ureq::AgentBuilder::new()
                .timeout(HTTP_REQUEST_TIMEOUT)
                .redirects(3)
                .build();
            let mut request = agent
                .request(method, url)
                .set("User-Agent", "RobloxClientResearch/local");
            if let Some(headers) = v["Headers"].as_object() {
                if headers.len() > 64 {
                    return Err("too many HTTP headers".into());
                }
                for (key, value) in headers {
                    let value = value.as_str().ok_or("header value must be text")?;
                    if key.len() + value.len() > 8192
                        || key.contains(['\r', '\n'])
                        || value.contains(['\r', '\n'])
                    {
                        return Err("invalid HTTP header".into());
                    }
                    request = request.set(key, value);
                }
            }
            let trace = if method == "GET" {
                response_trace_config()
            } else {
                None
            };
            let request_started = Instant::now();
            let result = if let Some(body) = v["BodyBase64"].as_str() {
                request.send_bytes(&STANDARD.decode(body).map_err(|e| e.to_string())?)
            } else {
                request.call()
            };
            let response = match result {
                Ok(r) => r,
                Err(ureq::Error::Status(_, r)) => r,
                Err(e) => return Err(e.to_string()),
            };
            if method != "HEAD"
                && response
                    .header("Content-Length")
                    .and_then(|value| value.parse::<u64>().ok())
                    .is_some_and(|length| length > HTTP_RESPONSE_LIMIT as u64)
            {
                return Err("HTTP body exceeds configured response limit".into());
            }
            let status = response.status();
            let message = response.status_text().to_string();
            let effective_url = response.get_url().to_string();
            let content_type = response.header("Content-Type").map(str::to_string);
            let mut headers = serde_json::Map::new();
            for name in response.headers_names() {
                if let Some(value) = response.header(&name) {
                    headers.insert(name, json!(value));
                }
            }
            let mut body = Vec::new();
            response
                .into_reader()
                .take(HTTP_RESPONSE_LIMIT as u64 + 1)
                .read_to_end(&mut body)
                .map_err(|e| e.to_string())?;
            if body.len() > HTTP_RESPONSE_LIMIT {
                return Err("HTTP body exceeds configured response limit".into());
            }
            if let Some(trace) = trace {
                if let Some(record) = response_trace_record(
                    method,
                    url,
                    &effective_url,
                    status,
                    content_type.as_deref(),
                    &body,
                    request_started.elapsed().as_millis(),
                    &trace.allowed_urls,
                ) {
                    append_http_trace(&trace.path, &record);
                }
            }
            Ok(
                json!({"StatusCode":status,"StatusMessage":message,"Success":(200..300).contains(&status),"Headers":headers,"BodyBase64":STANDARD.encode(body)}),
            )
        }
        "connect" => {
            let url = text(&v, "Url")?;
            if !(url.starts_with("wss://") || url.starts_with("ws://")) {
                return Err("WS(S) URL required".into());
            }
            if sockets()
                .lock()
                .map_err(|_| "socket pool lock failed")?
                .sockets
                .len()
                >= 16
            {
                return Err("WebSocket limit exceeded".into());
            }
            let socket = connect_socket_with_timeout(url, websocket_timeout)?;
            let mut pool = sockets().lock().map_err(|_| "socket pool lock failed")?;
            if pool.sockets.len() >= 16 {
                return Err("WebSocket limit exceeded".into());
            }
            pool.next += 1;
            let id = pool.next;
            pool.sockets.insert(id, socket);
            Ok(json!({"id":id}))
        }
        "send" | "poll" | "poll_ready" | "close" => {
            let id = v["id"].as_u64().ok_or("socket ID required")?;
            let poll_ready = v["operation"] == "poll_ready";
            let mut pool = if poll_ready {
                match sockets().try_lock() {
                    Ok(pool) => pool,
                    Err(TryLockError::WouldBlock) => return Ok(json!({"ready":false})),
                    Err(TryLockError::Poisoned(_)) => return Err("socket pool lock failed".into()),
                }
            } else {
                sockets().lock().map_err(|_| "socket pool lock failed")?
            };
            if v["operation"] == "close" {
                if let Some(mut socket) = pool.sockets.remove(&id) {
                    let _ = socket.close(None);
                }
                return Ok(json!({"closed":true}));
            }
            let socket = pool.sockets.get_mut(&id).ok_or("socket is closed")?;
            if v["operation"] == "send" {
                let data = STANDARD
                    .decode(text(&v, "BodyBase64")?)
                    .map_err(|e| e.to_string())?;
                let message = if v["binary"].as_bool().unwrap_or(false) {
                    Message::Binary(data.into())
                } else {
                    Message::Text(
                        String::from_utf8(data)
                            .map_err(|_| "text WebSocket message must be UTF-8")?
                            .into(),
                    )
                };
                socket.send(message).map_err(|e| e.to_string())?;
                return Ok(json!({}));
            }
            let poll_deadline = Instant::now() + WEBSOCKET_POLL_BUDGET;
            if poll_ready {
                if let Err(error) = set_poll_deadline(socket.get_mut(), poll_deadline) {
                    let _ = timeout(socket.get_mut());
                    return Err(error);
                }
            }
            let mut messages = Vec::new();
            let mut closed = false;
            let mut terminal_error = None;
            for _ in 0..16 {
                if poll_ready && Instant::now() >= poll_deadline {
                    break;
                }
                match socket.read() {
                    Ok(Message::Text(s)) => messages.push(STANDARD.encode(s.as_bytes())),
                    Ok(Message::Binary(s)) => messages.push(STANDARD.encode(s)),
                    Ok(Message::Close(_))
                    | Err(tungstenite::Error::ConnectionClosed)
                    | Err(tungstenite::Error::AlreadyClosed) => {
                        closed = true;
                        break;
                    }
                    Ok(_) => {
                        let _ = socket.flush();
                    }
                    Err(tungstenite::Error::Io(e))
                        if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                    {
                        break
                    }
                    Err(e) => {
                        terminal_error = Some(e.to_string());
                        break;
                    }
                }
            }
            if poll_ready {
                if let Err(error) = timeout(socket.get_mut()) {
                    if terminal_error.is_none() {
                        terminal_error = Some(error);
                    }
                }
            }
            if closed || terminal_error.is_some() {
                pool.sockets.remove(&id);
            }
            if let Some(error) = terminal_error {
                if messages.is_empty() {
                    if poll_ready {
                        return Ok(json!({"ready":true,"value":{"error":error}}));
                    }
                    return Err(error);
                }
                let value = json!({
                    "messages": messages,
                    "closed": true,
                    "transport_error": error,
                });
                return if poll_ready {
                    Ok(json!({"ready":true,"value":value}))
                } else {
                    Ok(value)
                };
            }
            let value = json!({"messages":messages,"closed":closed});
            if poll_ready {
                if closed || !value["messages"].as_array().is_some_and(Vec::is_empty) {
                    Ok(json!({"ready":true,"value":value}))
                } else {
                    Ok(json!({"ready":false}))
                }
            } else {
                Ok(value)
            }
        }
        _ => Err("unsupported network operation".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::mpsc,
        thread,
        time::Instant,
    };

    // Tests using the process-global WebSocket registry must not compare its
    // contents while another test is concurrently opening or closing sockets.
    // Keep this guard separate from the registry mutex: the lock-contention
    // test intentionally acquires the registry mutex itself.
    static SOCKET_REGISTRY_TEST_LOCK: Mutex<()> = Mutex::new(());

    fn socket_registry_test_guard() -> std::sync::MutexGuard<'static, ()> {
        SOCKET_REGISTRY_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn discarded_http_retains_capacity_until_worker_publishes() {
        let mut jobs = Jobs::default();
        for id in 1..=16 {
            jobs.pending.insert(
                id,
                NetworkJob {
                    operation: "http".into(),
                    result: None,
                    discarded: false,
                },
            );
            jobs.discard_http(id).unwrap();
        }
        assert_eq!(jobs.pending.len(), 16);
        assert!(jobs
            .pending
            .values()
            .all(|slot| slot.discarded && slot.result.is_none()));
        let _ = jobs.publish(1, json!({"BodyBase64":"bGF0ZQ=="}));
        assert_eq!(jobs.pending.len(), 15);
        assert!(!jobs.pending.contains_key(&1));
        let _ = jobs.publish(1, json!({"error":"late duplicate"}));
        assert!(!jobs.pending.contains_key(&1));
        jobs.discard_http(1).unwrap();
    }

    #[test]
    fn completed_http_discard_drops_retained_response_immediately() {
        let mut jobs = Jobs::default();
        jobs.pending.insert(
            1,
            NetworkJob {
                operation: "http".into(),
                result: None,
                discarded: false,
            },
        );
        let _ = jobs.publish(1, json!({"BodyBase64":"YWN0dWFs"}));
        assert!(jobs.pending[&1].result.is_some());
        jobs.discard_http(1).unwrap();
        assert!(jobs.pending.is_empty());
        jobs.discard_http(1).unwrap();
    }

    #[test]
    fn http_discard_preserves_other_async_operations() {
        let mut jobs = Jobs::default();
        jobs.pending.insert(
            1,
            NetworkJob {
                operation: "connect".into(),
                result: None,
                discarded: false,
            },
        );
        assert!(jobs.discard_http(1).is_err());
        let _ = jobs.publish(1, json!({"id":7}));
        assert_eq!(jobs.pending[&1].result, Some(json!({"id":7})));
        assert!(!jobs.pending[&1].discarded);
    }

    #[test]
    fn claiming_completed_connect_transfers_socket_ownership() {
        let mut jobs = Jobs::default();
        jobs.pending.insert(
            1,
            NetworkJob {
                operation: "connect".into(),
                result: Some(json!({"id":7})),
                discarded: false,
            },
        );
        jobs.claim_network_job(1).unwrap();
        assert!(jobs.pending.is_empty());
    }

    #[test]
    fn discarded_connect_closes_socket_after_pending_handshake_finishes() {
        let _serial = socket_registry_test_guard();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let (request_tx, request_rx) = mpsc::channel();
        let (continue_tx, continue_rx) = mpsc::channel();
        let (closed_tx, closed_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0_u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                if stream.read_exact(&mut byte).is_err() {
                    return;
                }
                request.push(byte[0]);
                if request.len() > 16 * 1024 {
                    return;
                }
            }
            let headers = String::from_utf8_lossy(&request);
            let key = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("sec-websocket-key")
                        .then(|| value.trim().to_owned())
                })
                .expect("client omitted Sec-WebSocket-Key");
            request_tx.send(key.clone()).unwrap();
            if continue_rx.recv_timeout(Duration::from_secs(1)).is_err() {
                return;
            }
            let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
            let response = format!(
                "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
            );
            if stream.write_all(response.as_bytes()).is_err() {
                return;
            }
            let mut byte = [0_u8; 16];
            let _ = stream.read(&mut byte);
            let _ = closed_tx.send(());
        });

        let started = operation(json!({
            "operation":"start",
            "request":{"operation":"connect","Url":format!("ws://{address}/cancel")}
        }))
        .unwrap();
        let job_id = started["id"].as_u64().unwrap();
        request_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker did not begin the WebSocket handshake");
        self::jobs()
            .lock()
            .unwrap()
            .discard_network_job(job_id)
            .unwrap();
        continue_tx.send(()).unwrap();
        closed_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("discarded connect did not close its newly-created socket");
        let state = self::jobs().lock().unwrap();
        assert!(!state.pending.contains_key(&job_id));
        drop(state);
        server.join().unwrap();
    }

    #[test]
    fn discarding_completed_connect_closes_unconsumed_socket() {
        let _serial = socket_registry_test_guard();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let _ = tungstenite::accept(stream);
        });
        let started = operation(json!({
            "operation":"start",
            "request":{"operation":"connect","Url":format!("ws://{address}/cancel-complete")}
        }))
        .unwrap();
        let job_id = started["id"].as_u64().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        let socket_id = loop {
            if let Some(result) = self::jobs().lock().unwrap().pending[&job_id]
                .result
                .as_ref()
            {
                break result["id"]
                    .as_u64()
                    .expect("connect job returned no socket ID");
            }
            assert!(Instant::now() < deadline, "connect worker did not finish");
            thread::sleep(Duration::from_millis(1));
        };
        operation(json!({"operation":"discard_network_job","id":job_id})).unwrap();
        assert!(!sockets().lock().unwrap().sockets.contains_key(&socket_id));
        server.join().unwrap();
    }

    fn response_server(body: Vec<u8>, delay: Duration) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 256];
            while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                let count = stream.read(&mut chunk).unwrap();
                assert!(count > 0, "client closed before sending HTTP headers");
                request.extend_from_slice(&chunk[..count]);
                assert!(
                    request.len() <= 16 * 1024,
                    "request headers exceeded fixture limit"
                );
            }
            if !delay.is_zero() {
                thread::sleep(delay);
            }
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            if stream.write_all(header.as_bytes()).is_ok() {
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
        });
        (format!("http://{address}/fixture"), server)
    }

    fn get(url: &str) -> Result<Value, String> {
        operation(json!({"operation":"http", "Url":url, "Method":"GET"}))
    }

    fn read_message_until(socket: &mut Socket, deadline: Instant) -> Message {
        loop {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for WebSocket echo"
            );
            match socket.read() {
                Ok(message) => return message,
                Err(tungstenite::Error::Io(error))
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("WebSocket read failed: {error}"),
            }
        }
    }

    #[test]
    fn websocket_follows_http_location_and_rebuilds_the_host_header() {
        let destination = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = destination.local_addr().unwrap();
        let echo = thread::spawn(move || {
            let (stream, _) = destination.accept().unwrap();
            let mut socket = tungstenite::accept_hdr(
                stream,
                |request: &tungstenite::handshake::server::Request, response| {
                    assert_eq!(
                        request.headers().get("Host").unwrap(),
                        address.to_string().as_str()
                    );
                    assert_eq!(request.uri().path(), "/echo");
                    Ok(response)
                },
            )
            .unwrap();
            let message = socket.read().unwrap();
            socket.send(message).unwrap();
        });
        let redirect = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let first = redirect.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = redirect.accept().unwrap();
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                let mut byte = [0u8; 1];
                stream.read_exact(&mut byte).unwrap();
                bytes.push(byte[0]);
                assert!(bytes.len() < 16384);
            }
            write!(stream, "HTTP/1.1 301 Moved Permanently\r\nLocation: http://{address}/echo\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        let mut socket = connect_socket(&format!("ws://{first}/start")).unwrap();
        socket.send(Message::Text("redirect echo".into())).unwrap();
        assert_eq!(
            read_message_until(&mut socket, Instant::now() + Duration::from_secs(2))
                .into_text()
                .unwrap(),
            "redirect echo"
        );
        server.join().unwrap();
        echo.join().unwrap();
    }

    #[test]
    fn websocket_normalizes_secure_and_relative_redirects() {
        let base = websocket_url("wss://example.test/first", None).unwrap();
        assert_eq!(
            websocket_url("https://other.test/echo", Some(&base))
                .unwrap()
                .as_str(),
            "wss://other.test/echo"
        );
        assert_eq!(
            websocket_url("/next", Some(&base)).unwrap().as_str(),
            "wss://example.test/next"
        );
        for invalid in [
            "file:///tmp/data",
            "ws://",
            "wss://example.test/#fragment",
            "wss://user:pass@example.test/",
        ] {
            assert!(websocket_url(invalid, None).is_err(), "{invalid}");
        }
    }

    fn silent_websocket_server() -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = thread::spawn(move || {
            let accept_deadline = Instant::now() + Duration::from_secs(1);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error)
                        if error.kind() == ErrorKind::WouldBlock
                            && Instant::now() < accept_deadline =>
                    {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => return,
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut buffer = [0_u8; 1024];
            // Read the upgrade request, then remain silent until the timed-out
            // client closes. This models a peer that accepts TCP but stalls
            // before completing the HTTP WebSocket handshake.
            while stream.read(&mut buffer).unwrap_or(0) > 0 {}
        });
        (format!("ws://{address}/silent"), server)
    }

    #[test]
    fn websocket_silent_handshake_times_out_without_peer_close() {
        let (url, server) = silent_websocket_server();
        let started = Instant::now();
        let result = connect_socket_with_timeout(&url, Duration::from_millis(150));
        let elapsed = started.elapsed();
        assert!(
            result.is_err(),
            "silent peer unexpectedly completed handshake"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "handshake exceeded bounded test window: {elapsed:?}"
        );
        assert!(
            elapsed >= Duration::from_millis(100),
            "peer closed before deadline: {elapsed:?}"
        );
        server.join().unwrap();
    }

    #[test]
    fn failed_websocket_handshake_does_not_consume_socket_capacity() {
        let _serial = socket_registry_test_guard();
        let (url, server) = silent_websocket_server();
        let before = sockets().lock().unwrap().sockets.len();
        let started = Instant::now();
        let result = operation_with_ws_timeout(
            json!({"operation":"connect", "Url":url}),
            Duration::from_millis(150),
        );
        let after = sockets().lock().unwrap().sockets.len();
        let elapsed = started.elapsed();
        server.join().unwrap();
        assert!(
            result.is_err(),
            "silent peer unexpectedly completed handshake"
        );
        assert_eq!(
            after, before,
            "failed handshake left a socket in the registry"
        );
        assert!(
            elapsed >= Duration::from_millis(100),
            "peer closed before deadline: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "handshake failed to time out: {elapsed:?}"
        );
    }

    #[test]
    fn websocket_redirects_share_one_absolute_handshake_deadline() {
        let target = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let target_address = target.local_addr().unwrap();
        target.set_nonblocking(true).unwrap();
        let target_server = thread::spawn(move || {
            let accept_deadline = Instant::now() + Duration::from_secs(1);
            let Ok((mut stream, _)) = (|| loop {
                match target.accept() {
                    Ok(connection) => break Ok(connection),
                    Err(error)
                        if error.kind() == ErrorKind::WouldBlock
                            && Instant::now() < accept_deadline =>
                    {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => break Err(error),
                }
            })() else {
                return;
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut buffer = [0_u8; 1024];
            while stream.read(&mut buffer).unwrap_or(0) > 0 {}
        });

        let redirect = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let redirect_address = redirect.local_addr().unwrap();
        redirect.set_nonblocking(true).unwrap();
        let redirect_server = thread::spawn(move || {
            let accept_deadline = Instant::now() + Duration::from_secs(1);
            let Ok((mut stream, _)) = (|| loop {
                match redirect.accept() {
                    Ok(connection) => break Ok(connection),
                    Err(error)
                        if error.kind() == ErrorKind::WouldBlock
                            && Instant::now() < accept_deadline =>
                    {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => break Err(error),
                }
            })() else {
                return;
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0_u8; 1];
            while !request.ends_with(b"\r\n\r\n") {
                match stream.read_exact(&mut byte) {
                    Ok(()) => request.push(byte[0]),
                    Err(_) => return,
                }
                if request.len() > 16 * 1024 {
                    return;
                }
            }
            thread::sleep(Duration::from_millis(350));
            let _ = write!(
                stream,
                "HTTP/1.1 301 Moved Permanently\r\nLocation: ws://{target_address}/silent\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            );
        });

        let started = Instant::now();
        let result = connect_socket_with_timeout(
            &format!("ws://{redirect_address}/redirect"),
            Duration::from_millis(500),
        );
        let elapsed = started.elapsed();
        assert!(
            result.is_err(),
            "silent redirected peer unexpectedly completed handshake"
        );
        assert!(
            elapsed < Duration::from_millis(700),
            "redirect reset the total deadline: {elapsed:?}"
        );
        assert!(
            elapsed >= Duration::from_millis(450),
            "redirected peer closed before deadline: {elapsed:?}"
        );
        redirect_server.join().unwrap();
        target_server.join().unwrap();
    }

    #[test]
    fn websocket_connects_over_ipv6_loopback_when_available() {
        let Ok(listener) = TcpListener::bind(("::1", 0)) else {
            return;
        };
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let server = thread::spawn(move || {
            let accept_deadline = Instant::now() + Duration::from_secs(1);
            let (stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error)
                        if error.kind() == ErrorKind::WouldBlock
                            && Instant::now() < accept_deadline =>
                    {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => return,
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            let message = socket.read().unwrap();
            socket.send(message).unwrap();
        });
        let mut socket = connect_socket(&format!("ws://{address}/echo")).unwrap();
        socket.send(Message::Text("ipv6 echo".into())).unwrap();
        assert_eq!(
            read_message_until(&mut socket, Instant::now() + Duration::from_secs(2))
                .into_text()
                .unwrap(),
            "ipv6 echo"
        );
        server.join().unwrap();
    }

    fn poll_after_terminal_frames(frames: Vec<u8>) -> (u64, Result<Value, String>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (send_tx, send_rx) = mpsc::channel();
        let (wrote_tx, wrote_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let accept_deadline = Instant::now() + Duration::from_secs(1);
            let (stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error)
                        if error.kind() == ErrorKind::WouldBlock
                            && Instant::now() < accept_deadline =>
                    {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => return,
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            ready_tx.send(()).unwrap();
            if send_rx.recv_timeout(Duration::from_secs(1)).is_err() {
                return;
            }
            socket.get_mut().write_all(&frames).unwrap();
            socket.get_mut().flush().unwrap();
            wrote_tx.send(()).unwrap();
            thread::sleep(Duration::from_millis(100));
        });

        let connected = operation_with_ws_timeout(
            json!({"operation":"connect", "Url":format!("ws://{address}/malformed")}),
            Duration::from_secs(1),
        )
        .unwrap();
        let id = connected["id"].as_u64().unwrap();
        ready_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("loopback peer did not complete its upgrade");
        let waiting = operation(json!({"operation":"poll", "id":id})).unwrap();
        assert_eq!(waiting["closed"], false);
        assert!(
            sockets().lock().unwrap().sockets.contains_key(&id),
            "transient poll timeout consumed the socket"
        );
        send_tx.send(()).unwrap();
        wrote_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("loopback peer did not finish writing its frames");
        let result = operation(json!({"operation":"poll", "id":id}));
        server.join().unwrap();
        (id, result)
    }

    fn poll_ready_after_frames(frames: Vec<u8>) -> (u64, Vec<Value>) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (send_tx, send_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(1);
            let (stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error)
                        if error.kind() == ErrorKind::WouldBlock && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => return,
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            ready_tx.send(()).unwrap();
            if send_rx.recv_timeout(Duration::from_secs(1)).is_ok() {
                socket.get_mut().write_all(&frames).unwrap();
                socket.get_mut().flush().unwrap();
            }
        });
        let connected = operation(json!({
            "operation":"connect",
            "Url":format!("ws://{address}/poll-ready")
        }))
        .unwrap();
        let id = connected["id"].as_u64().unwrap();
        ready_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("loopback peer did not finish upgrade");
        assert_eq!(
            operation(json!({"operation":"poll_ready","id":id})).unwrap(),
            json!({"ready":false})
        );
        send_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut results = Vec::new();
        loop {
            let result = operation(json!({"operation":"poll_ready","id":id})).unwrap();
            if result["ready"] == true {
                let terminal = result["value"]["closed"] == true
                    || result["value"]["transport_error"].as_str().is_some()
                    || result["value"]["error"].as_str().is_some();
                results.push(result);
                if terminal {
                    break;
                }
            }
            assert!(
                Instant::now() < deadline,
                "poll_ready did not observe all peer frames and the terminal result"
            );
            thread::sleep(Duration::from_millis(1));
        }
        server.join().unwrap();
        (id, results)
    }

    #[test]
    fn poll_ready_returns_pending_when_socket_pool_lock_is_busy() {
        let _serial = socket_registry_test_guard();
        let guard = sockets().lock().unwrap();
        assert_eq!(
            operation(json!({"operation":"poll_ready","id":1})).unwrap(),
            json!({"ready":false})
        );
        drop(guard);
    }

    #[test]
    fn poll_ready_delivers_real_messages_and_terminal_errors() {
        let _serial = socket_registry_test_guard();
        // TCP/WebSocket buffering can make the message and protocol error
        // visible in one read batch or two. Preserve and check every ready
        // payload until the terminal error is observed.
        let (id, results) = poll_ready_after_frames(vec![0x81, 0x02, b'h', b'i', 0x83, 0x00]);
        let values: Vec<_> = results.iter().map(|result| &result["value"]).collect();
        assert_eq!(
            STANDARD
                .decode(
                    values
                        .iter()
                        .find_map(|value| value["messages"][0].as_str())
                        .expect("text message was not delivered")
                )
                .unwrap(),
            b"hi".to_vec()
        );
        assert!(values
            .iter()
            .any(|value| value["transport_error"].as_str().is_some()));
        assert!(!sockets().lock().unwrap().sockets.contains_key(&id));

        let (id, results) = poll_ready_after_frames(vec![0x83, 0x00]);
        assert!(results
            .iter()
            .any(|result| result["value"]["error"].as_str().is_some()));
        assert!(!sockets().lock().unwrap().sockets.contains_key(&id));
    }

    #[test]
    fn poll_ready_preserves_fragmented_message_across_bounded_control_polls() {
        let _serial = socket_registry_test_guard();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (frame_tx, frame_rx) = mpsc::channel::<Vec<u8>>();
        let (sent_tx, sent_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let accept_deadline = Instant::now() + Duration::from_secs(1);
            let (stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error)
                        if error.kind() == ErrorKind::WouldBlock
                            && Instant::now() < accept_deadline =>
                    {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => return,
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            ready_tx.send(()).unwrap();
            for _ in 0..3 {
                let Ok(frame) = frame_rx.recv_timeout(Duration::from_secs(1)) else {
                    return;
                };
                if socket.get_mut().write_all(&frame).is_err() {
                    return;
                }
                if sent_tx.send(()).is_err() {
                    return;
                }
            }
            let _ = release_rx.recv_timeout(Duration::from_secs(2));
        });

        let connected = operation(json!({
            "operation":"connect",
            "Url":format!("ws://{address}/fragmented-poll")
        }))
        .unwrap();
        let id = connected["id"].as_u64().unwrap();
        ready_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("loopback peer did not finish upgrade");

        for frame in [
            vec![0x01, 0x03, b'H', b'e', b'l'], // non-final text fragment
            vec![0x89, 0x01, b'!'],             // interleaved ping
        ] {
            let started = Instant::now();
            frame_tx.send(frame).unwrap();
            sent_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("peer did not send the next frame");
            let pending = operation(json!({"operation":"poll_ready","id":id})).unwrap();
            assert_eq!(pending, json!({"ready":false}));
            assert!(
                started.elapsed() < Duration::from_millis(100),
                "poll_ready exceeded its bounded poll window"
            );
            assert!(sockets().lock().unwrap().sockets.contains_key(&id));
        }

        frame_tx.send(vec![0x80, 0x02, b'l', b'o']).unwrap(); // final continuation
        sent_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("peer did not send the final continuation");
        let deadline = Instant::now() + Duration::from_secs(1);
        let result = loop {
            let result = operation(json!({"operation":"poll_ready","id":id})).unwrap();
            if result["ready"] == true {
                break result;
            }
            assert!(
                Instant::now() < deadline,
                "fragmented message was not completed"
            );
            thread::sleep(Duration::from_millis(1));
        };
        assert_eq!(
            STANDARD
                .decode(result["value"]["messages"][0].as_str().unwrap())
                .unwrap(),
            b"Hello".to_vec()
        );
        assert_eq!(result["value"]["closed"], false);
        release_tx.send(()).unwrap();
        remove_socket(id);
        server.join().unwrap();
    }

    #[test]
    fn terminal_poll_error_preserves_prior_message_and_removes_socket() {
        let _serial = socket_registry_test_guard();
        // A complete message followed by invalid opcode 3 forces both reads
        // into one poll batch.
        let (id, result) =
            poll_after_terminal_frames(vec![0x81, 0x04, b'k', b'e', b'p', b't', 0x83, 0x00]);
        let result = result.expect("completed message plus terminal error should be returned");
        assert_eq!(result["messages"].as_array().unwrap().len(), 1);
        assert_eq!(
            STANDARD
                .decode(result["messages"][0].as_str().unwrap())
                .unwrap(),
            b"kept".to_vec()
        );
        assert_eq!(result["closed"], true);
        let error = result["transport_error"].as_str().unwrap();
        assert!(
            error.to_lowercase().contains("invalid opcode"),
            "poll did not preserve the terminal protocol error: {error}"
        );
        assert!(
            !sockets().lock().unwrap().sockets.contains_key(&id),
            "terminal poll error left a dead socket in the registry"
        );
    }

    #[test]
    fn terminal_poll_error_without_message_returns_error_and_removes_socket() {
        let _serial = socket_registry_test_guard();
        let (id, result) = poll_after_terminal_frames(vec![0x83, 0x00]);
        let error = result.expect_err("terminal error without completed messages must remain Err");
        assert!(
            error.to_lowercase().contains("invalid opcode"),
            "poll did not preserve the original protocol error: {error}"
        );
        assert!(
            !sockets().lock().unwrap().sockets.contains_key(&id),
            "terminal poll error left a dead socket in the registry"
        );
    }

    #[test]
    fn http_reads_multi_megabyte_api_dump_responses() {
        let expected = vec![b'x'; 2 * 1024 * 1024 + 37];
        let (url, server) = response_server(expected.clone(), Duration::ZERO);
        let result = get(&url).unwrap();
        server.join().unwrap();
        assert_eq!(
            STANDARD
                .decode(result["BodyBase64"].as_str().unwrap())
                .unwrap(),
            expected
        );
    }

    #[test]
    fn http_rejects_responses_above_the_configured_limit() {
        let (url, server) = response_server(vec![b'x'; HTTP_RESPONSE_LIMIT + 1], Duration::ZERO);
        let error = get(&url).unwrap_err();
        server.join().unwrap();
        assert!(error.contains("response limit"), "{error}");
    }

    #[test]
    fn http_allows_normal_responses_to_take_longer_than_the_old_timeout() {
        let (url, server) = response_server(b"ok".to_vec(), Duration::from_millis(1500));
        let started = Instant::now();
        let result = get(&url).unwrap();
        server.join().unwrap();
        assert_eq!(
            STANDARD
                .decode(result["BodyBase64"].as_str().unwrap())
                .unwrap(),
            b"ok"
        );
        assert!(started.elapsed() >= Duration::from_millis(1400));
    }

    fn trace_test_path() -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "rcr-http-trace-{}-{nonce}.jsonl",
            std::process::id()
        ))
    }

    #[test]
    fn response_trace_hashes_binary_body_without_changing_it() {
        let original = b"\0\xffmutable";
        let before = original.to_vec();
        let allowed = ["https://tests.invalid/mutable".to_string()]
            .into_iter()
            .collect();
        let record = response_trace_record(
            "GET",
            "https://tests.invalid/mutable",
            "https://tests.invalid/mutable",
            200,
            Some("application/octet-stream"),
            original,
            37,
            &allowed,
        )
        .unwrap();
        assert_eq!(original.as_slice(), before.as_slice());
        assert_eq!(
            record["body_sha256"],
            "bbff0ea0cb0d251541e56a5c12463733f3ab72c73dc66c5ea0614d3717552403"
        );
        assert_eq!(record["body_length"], original.len());
        assert_eq!(record["status"], 200);
        assert_eq!(record["content_type"], "application/octet-stream");
        assert!(record.get("body").is_none());
        assert!(record.get("headers").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn response_trace_appends_one_json_record_with_only_approved_metadata() {
        let path = trace_test_path();
        let body = b"TRACE_PRIVATE_BODY_SENTINEL";
        let allowed = ["https://tests.invalid/mutable".to_string()]
            .into_iter()
            .collect();
        let record = response_trace_record(
            "GET",
            "https://tests.invalid/mutable",
            "https://tests.invalid/mutable",
            200,
            Some("application/octet-stream"),
            body,
            37,
            &allowed,
        )
        .unwrap();

        append_http_trace(&path, &record);

        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 1);
        let written: Value = serde_json::from_str(text.trim_end()).unwrap();
        let keys: HashSet<&str> = written
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys,
            [
                "requested_url",
                "effective_url",
                "unix_ms",
                "status",
                "content_type",
                "body_length",
                "body_sha256",
                "elapsed_ms"
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(written["body_length"], body.len());
        assert!(written["unix_ms"].as_u64().is_some());
        assert!(!text.contains(std::str::from_utf8(body).unwrap()));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn response_trace_rejects_non_allowlisted_redirects_and_non_gets() {
        let allowed = ["https://tests.invalid/mutable".to_string()]
            .into_iter()
            .collect();
        let path = trace_test_path();
        for (method, requested, effective) in [
            (
                "GET",
                "https://tests.invalid/other",
                "https://tests.invalid/other",
            ),
            (
                "GET",
                "https://tests.invalid/mutable",
                "https://elsewhere.invalid/mutable",
            ),
            (
                "GET",
                "https://user:pass@tests.invalid/mutable",
                "https://tests.invalid/mutable",
            ),
            (
                "GET",
                "https://tests.invalid/mutable?private=1",
                "https://tests.invalid/mutable?private=1",
            ),
            (
                "POST",
                "https://tests.invalid/mutable",
                "https://tests.invalid/mutable",
            ),
        ] {
            assert!(response_trace_record(
                method, requested, effective, 200, None, b"hidden", 1, &allowed
            )
            .is_none());
        }
        assert!(!path.exists(), "rejected URLs must not create a trace file");
    }

    #[cfg(unix)]
    #[test]
    fn response_trace_file_is_owner_only_and_bounded() {
        use std::os::unix::fs::PermissionsExt;
        let path = trace_test_path();
        fs::write(&path, vec![b'x'; HTTP_TRACE_LIMIT as usize - 4]).unwrap();
        let record = json!({"status":200,"body_sha256":"abc"});
        append_http_trace(&path, &record);
        let metadata = fs::metadata(&path).unwrap();
        assert_eq!(metadata.len(), HTTP_TRACE_LIMIT - 4);
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        let _ = fs::remove_file(path);
    }
}
pub fn apply(input: &[u8]) -> Vec<u8> {
    let result = serde_json::from_slice(input)
        .map_err(|e| e.to_string())
        .and_then(operation);
    serde_json::to_vec(&match result {
        Ok(v) => v,
        Err(e) => json!({"error":e}),
    })
    .unwrap_or_else(|_| b"{\"error\":\"response encoding failed\"}".to_vec())
}
