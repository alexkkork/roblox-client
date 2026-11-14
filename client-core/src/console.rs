//! Bounded, owner-only local console transport and process session lease.
//!
//! The transport accepts one newline-delimited JSON request per local connection.
//! Unix uses an owner-only socket; Windows uses authenticated loopback TCP
//! with a private endpoint file. It keeps at most one peer and reads/writes small slices per
//! poll so a large request cannot monopolize the client loop.

use serde_json::{json, Value};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub const MAX_SOURCE_BYTES: usize = 100 * 1024 * 1024;
pub const MAX_REQUEST_BYTES: usize = 104 * 1024 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const READ_SLICE: usize = 64 * 1024;
const MAX_READ_PER_POLL: usize = 256 * 1024;
const MAX_WRITE_PER_POLL: usize = 256 * 1024;
const PEER_TTL: Duration = Duration::from_secs(60);

#[cfg(windows)]
use std::net::{TcpListener as LocalListener, TcpStream as LocalStream};
#[cfg(unix)]
use std::os::unix::net::{UnixListener as LocalListener, UnixStream as LocalStream};

#[derive(Debug)]
pub struct SessionLease {
    _file: File,
}

impl SessionLease {
    /// Hold `root/secrets/client-session.lock` until this value is dropped.
    /// The file intentionally remains on disk; the OS lock belongs to the FD.
    pub fn acquire(root: impl AsRef<Path>) -> Result<Self, String> {
        #[cfg(unix)]
        {
            use std::os::unix::{
                fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
                io::AsRawFd,
            };

            let secrets = root.as_ref().join("secrets");
            let directory = fs::symlink_metadata(&secrets)
                .map_err(|_| "Client secrets directory is unavailable".to_owned())?;
            if !directory.file_type().is_dir()
                || directory.uid() != unsafe { libc::geteuid() }
                || directory.permissions().mode() & 0o077 != 0
            {
                return Err("Client secrets directory must be owned and private".into());
            }
            let path = secrets.join("client-session.lock");
            let mut options = OpenOptions::new();
            options.read(true).write(true).create(true).mode(0o600);
            options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);

            let file = options
                .open(&path)
                .map_err(|_| "Cannot safely open the client session lease".to_owned())?;
            let metadata = file
                .metadata()
                .map_err(|_| "Cannot inspect the client session lease")?;
            if !metadata.file_type().is_file() || metadata.uid() != unsafe { libc::geteuid() } {
                return Err("Client session lease must be an owned regular file".into());
            }
            file.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|_| "Cannot protect the client session lease".to_owned())?;
            // SAFETY: `file` owns a live descriptor. LOCK_EX|LOCK_NB is the
            // documented flock operation; it does not alter file contents.
            let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if result != 0 {
                return Err("Another client session already holds the account lease".into());
            }
            Ok(Self { _file: file })
        }
        #[cfg(windows)]
        {
            let secrets = root.as_ref().join("secrets");
            let _directory = crate::windows_private::create_directory(&secrets)
                .map_err(|_| "Client secrets directory must have a private Windows ACL")?;
            let file = crate::windows_private::session_lock(&secrets.join("client-session.lock"))
                .map_err(|_| {
                "Cannot acquire the account lease; another session may be running"
            })?;
            Ok(Self { _file: file })
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = root;
            Err("Safe client session leases are unsupported on this platform".into())
        }
    }
}

#[cfg(any(unix, windows))]
#[derive(Debug)]
struct Peer {
    stream: LocalStream,
    data: Vec<u8>,
    response: Option<Vec<u8>>,
    written: usize,
    connected_at: Instant,
}

/// One-request-per-connection local script console.
#[cfg(any(unix, windows))]
#[derive(Debug)]
pub struct ScriptConsole {
    #[cfg(unix)]
    path: PathBuf,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    _endpoint: File,
    #[cfg(windows)]
    _directory: File,
    listener: LocalListener,
    peer: Option<Peer>,
    auth_token: Option<String>,
    requests: u64,
}

#[cfg(any(unix, windows))]
impl ScriptConsole {
    /// Bind an owner-only Unix socket. Unix endpoints do not require a token;
    /// pass one to enforce a token on every request when desired.
    #[cfg(unix)]
    pub fn bind(path: impl AsRef<Path>, auth_token: Option<String>) -> Result<Self, String> {
        use std::os::unix::{
            fs::{FileTypeExt, MetadataExt, PermissionsExt},
            net::UnixListener,
        };
        let path = path.as_ref().to_path_buf();
        let parent = path
            .parent()
            .ok_or("Console socket needs a parent directory")?;
        let parent_meta = fs::symlink_metadata(parent)
            .map_err(|_| "Console socket parent directory is unavailable".to_owned())?;
        if !parent_meta.file_type().is_dir()
            || parent_meta.uid() != unsafe { libc::geteuid() }
            || parent_meta.permissions().mode() & 0o077 != 0
        {
            return Err("Console socket parent must be an owned private directory".into());
        }

        if let Ok(existing) = fs::symlink_metadata(&path) {
            if !existing.file_type().is_socket() || existing.uid() != unsafe { libc::geteuid() } {
                return Err("Console path is not an owned socket".into());
            }
            match std::os::unix::net::UnixStream::connect(&path) {
                Ok(stream) => {
                    drop(stream); // probe only; never send a request or unlink it
                    return Err("A client console is already running".into());
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
                    ) =>
                {
                    // Remove only the same stale owned socket observed above.
                    let current = fs::symlink_metadata(&path)
                        .map_err(|_| "Console path changed during stale socket check".to_owned())?;
                    if current.ino() != existing.ino()
                        || current.uid() != existing.uid()
                        || !current.file_type().is_socket()
                    {
                        return Err("Console path changed during stale socket check".into());
                    }
                    fs::remove_file(&path)
                        .map_err(|_| "Cannot remove stale console socket".to_owned())?;
                }
                Err(_) => return Err("An existing client console may still be active".into()),
            }
        }

        let listener =
            UnixListener::bind(&path).map_err(|_| "Cannot bind local client console".to_owned())?;
        if let Err(_) = fs::set_permissions(&path, fs::Permissions::from_mode(0o600)) {
            let _ = fs::remove_file(&path);
            return Err("Cannot protect local client console socket".into());
        }
        if let Err(_) = listener.set_nonblocking(true) {
            let _ = fs::remove_file(&path);
            return Err("Cannot configure local client console".into());
        }
        let inode = fs::symlink_metadata(&path)
            .map_err(|_| "Cannot inspect local client console socket".to_owned())?
            .ino();
        Ok(Self {
            path,
            inode,
            listener,
            peer: None,
            auth_token,
            requests: 0,
        })
    }

    /// Windows publishes a token and ephemeral loopback port in a private file.
    #[cfg(windows)]
    pub fn bind(path: impl AsRef<Path>, auth_token: Option<String>) -> Result<Self, String> {
        let path = path.as_ref();
        let parent = path
            .parent()
            .ok_or("Console endpoint needs a parent directory")?;
        let directory = crate::windows_private::open_directory(parent)
            .map_err(|_| "Console endpoint parent must have a private Windows ACL")?;
        let token = match auth_token {
            Some(token) if token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()) => {
                token
            }
            Some(_) => return Err("Windows console token must be 64 hexadecimal characters".into()),
            None => {
                let mut bytes = [0; 32];
                crate::entropy::fill(&mut bytes).map_err(|_| "OS entropy unavailable")?;
                bytes.iter().map(|b| format!("{b:02x}")).collect()
            }
        };
        let listener = LocalListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .map_err(|_| "Cannot bind local client console")?;
        listener
            .set_nonblocking(true)
            .map_err(|_| "Cannot configure local client console")?;
        let port = listener
            .local_addr()
            .map_err(|_| "Cannot inspect local client console")?
            .port();
        let mut endpoint = crate::windows_private::create_file(path, true).map_err(|_| {
            "Cannot create private console endpoint; the path may already be in use"
        })?;
        serde_json::to_writer(
            &mut endpoint,
            &json!({"host":"127.0.0.1","port":port,"token":token}),
        )
        .map_err(|_| "Cannot write console endpoint")?;
        endpoint
            .flush()
            .map_err(|_| "Cannot flush console endpoint")?;
        Ok(Self {
            _endpoint: endpoint,
            _directory: directory,
            listener,
            peer: None,
            auth_token: Some(token),
            requests: 0,
        })
    }

    pub fn requests_handled(&self) -> u64 {
        self.requests
    }

    /// Whether a client request or response still needs servicing. In
    /// particular, dispatching `leave` is not proof its reply has been sent.
    pub fn has_pending_peer(&self) -> bool {
        self.peer.is_some()
    }

    /// Accept/read/dispatch/write a bounded slice. Returns whether one request
    /// was dispatched. The caller invokes this during its normal event loop.
    pub fn poll<F>(&mut self, dispatch: &mut F) -> Result<bool, String>
    where
        F: FnMut(Value) -> Result<Value, String>,
    {
        if self.peer.is_none() {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    stream
                        .set_nonblocking(true)
                        .map_err(|_| "Cannot configure console peer")?;
                    self.peer = Some(Peer {
                        stream,
                        data: Vec::new(),
                        response: None,
                        written: 0,
                        connected_at: Instant::now(),
                    });
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(_) => return Err("Cannot accept local console connection".into()),
            }
        }
        let Some(peer) = self.peer.as_mut() else {
            return Ok(false);
        };
        if peer.connected_at.elapsed() > PEER_TTL {
            self.peer = None;
            return Ok(false);
        }

        if peer.response.is_none() {
            let mut chunk = [0u8; READ_SLICE];
            match peer
                .stream
                .read(&mut chunk[..READ_SLICE.min(MAX_READ_PER_POLL)])
            {
                Ok(0) => {
                    self.peer = None;
                    return Ok(false);
                }
                Ok(count) => {
                    if peer.data.len().saturating_add(count) > MAX_REQUEST_BYTES {
                        peer.response = Some(error_response("Source request exceeds size limit"));
                    } else {
                        peer.data.extend_from_slice(&chunk[..count]);
                        if let Some(newline) = peer.data.iter().position(|byte| *byte == b'\n') {
                            if newline + 1 != peer.data.len() {
                                peer.response =
                                    Some(error_response("One request per connection is supported"));
                            } else {
                                let request = parse_request(
                                    &peer.data[..newline],
                                    self.auth_token.as_deref(),
                                );
                                let response = match request {
                                    Ok(request) => match dispatch(request) {
                                        Ok(value) => encode_response(&value).unwrap_or_else(|_| {
                                            error_response("Script response exceeds size limit")
                                        }),
                                        Err(error) => error_response(&error),
                                    },
                                    Err(error) => error_response(&error),
                                };
                                peer.response = Some(response);
                                self.requests = self.requests.saturating_add(1);
                                return Ok(true);
                            }
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(_) => {
                    self.peer = None;
                    return Ok(false);
                }
            }
        }

        if let Some(response) = &peer.response {
            let end = response
                .len()
                .min(peer.written.saturating_add(MAX_WRITE_PER_POLL));
            match peer.stream.write(&response[peer.written..end]) {
                Ok(0) => self.peer = None,
                Ok(count) => {
                    peer.written += count;
                    if peer.written >= response.len() {
                        self.peer = None;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => self.peer = None,
            }
        }
        Ok(false)
    }
}

#[cfg(unix)]
impl Drop for ScriptConsole {
    fn drop(&mut self) {
        if let Ok(metadata) = fs::symlink_metadata(&self.path) {
            use std::os::unix::fs::{FileTypeExt, MetadataExt};
            if metadata.ino() == self.inode
                && metadata.file_type().is_socket()
                && metadata.uid() == unsafe { libc::geteuid() }
            {
                let _ = fs::remove_file(&self.path);
            }
        }
    }
}

#[cfg(not(any(unix, windows)))]
#[derive(Debug)]
pub struct ScriptConsole;

#[cfg(not(any(unix, windows)))]
impl ScriptConsole {
    pub fn bind(_path: impl AsRef<Path>, _auth_token: Option<String>) -> Result<Self, String> {
        Err("Safe local script consoles are unsupported on this platform".into())
    }
}

fn parse_request(line: &[u8], token: Option<&str>) -> Result<Value, String> {
    let mut request: Value =
        serde_json::from_slice(line).map_err(|_| "Invalid source request".to_owned())?;
    let object = request.as_object_mut().ok_or("Invalid source request")?;
    if let Some(expected) = token {
        let supplied = object
            .remove("_console_token")
            .and_then(|v| v.as_str().map(str::to_owned))
            .ok_or("Console authentication failed")?;
        if !constant_time_equal(supplied.as_bytes(), expected.as_bytes()) {
            return Err("Console authentication failed".into());
        }
    }
    // The common script command receives stricter checks before it reaches the
    // host. Other operation shapes belong to the dispatcher's own validator.
    if object.contains_key("source") {
        if object
            .get("op")
            .is_some_and(|op| !op.is_null() && op.as_str() != Some("execute"))
        {
            return Err("Invalid script request".into());
        }
        let source = object
            .get("source")
            .and_then(Value::as_str)
            .ok_or("Invalid script request")?;
        if source.trim().is_empty() || source.len() > MAX_SOURCE_BYTES {
            return Err("Invalid script source".into());
        }
        let timeout = object
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .ok_or("Invalid script timeout")?;
        if !(1..=2000).contains(&timeout) {
            return Err("Invalid script timeout".into());
        }
        for key in object.keys() {
            if !matches!(key.as_str(), "op" | "source" | "timeout_ms" | "job_id") {
                return Err("Malformed script request".into());
            }
        }
    }
    Ok(request)
}

fn encode_response(value: &Value) -> Result<Vec<u8>, String> {
    let mut data = serde_json::to_vec(value).map_err(|_| "Invalid console result".to_owned())?;
    data.push(b'\n');
    if data.len() > MAX_RESPONSE_BYTES {
        return Err("Script response exceeds size limit".into());
    }
    Ok(data)
}

fn error_response(message: &str) -> Vec<u8> {
    let safe = if message.len() > 4096 {
        let end = message
            .char_indices()
            .map(|(index, _)| index)
            .take_while(|index| *index <= 4096)
            .last()
            .unwrap_or(0);
        &message[..end]
    } else {
        message
    };
    encode_response(&json!({"ok": false, "values": [], "error_type": "ValueError", "error": safe}))
        .unwrap_or_else(|_| b"{\"ok\":false,\"values\":[],\"error\":\"Console error\"}\n".to_vec())
}

fn constant_time_equal(a: &[u8], b: &[u8]) -> bool {
    let mut diff = a.len() ^ b.len();
    for index in 0..a.len().max(b.len()) {
        diff |=
            usize::from(a.get(index).copied().unwrap_or(0) ^ b.get(index).copied().unwrap_or(0));
    }
    diff == 0
}

/// Send a JSON request over the local console and read its bounded response.
#[cfg(any(unix, windows))]
pub fn send_request(
    path: impl AsRef<Path>,
    request: Value,
    timeout: Duration,
) -> Result<Value, String> {
    send_request_inner(path.as_ref(), request, timeout)
}

/// Send a request to a listener configured with a local bearer token.
#[cfg(any(unix, windows))]
pub fn send_request_with_token(
    path: impl AsRef<Path>,
    mut request: Value,
    token: &str,
    timeout: Duration,
) -> Result<Value, String> {
    let object = request.as_object_mut().ok_or("Invalid console request")?;
    if object.contains_key("_console_token") {
        return Err("Console token field is reserved".into());
    }
    object.insert("_console_token".into(), Value::String(token.to_owned()));
    send_request_inner(path.as_ref(), request, timeout)
}

#[cfg(any(unix, windows))]
fn send_request_inner(path: &Path, request: Value, timeout: Duration) -> Result<Value, String> {
    use std::net::Shutdown;
    #[cfg(unix)]
    let mut stream = LocalStream::connect(path).map_err(|_| "Cannot connect to local console")?;
    #[cfg(windows)]
    let (mut stream, request) = {
        let file = crate::windows_private::open_read(path, 4096)
            .map_err(|_| "Cannot read private console endpoint")?;
        let mut bytes = Vec::new();
        file.take(4097)
            .read_to_end(&mut bytes)
            .map_err(|_| "Cannot read private console endpoint")?;
        if bytes.len() > 4096 {
            return Err("Console endpoint exceeds size limit".into());
        }
        let endpoint: Value =
            serde_json::from_slice(&bytes).map_err(|_| "Invalid console endpoint")?;
        if endpoint.get("host").and_then(Value::as_str) != Some("127.0.0.1") {
            return Err("Console endpoint must be local".into());
        }
        let port = endpoint
            .get("port")
            .and_then(Value::as_u64)
            .and_then(|port| u16::try_from(port).ok())
            .filter(|port| *port != 0)
            .ok_or("Invalid console endpoint port")?;
        let token = endpoint
            .get("token")
            .and_then(Value::as_str)
            .filter(|token| token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or("Invalid console endpoint token")?;
        let mut request = request;
        request
            .as_object_mut()
            .ok_or("Invalid console request")?
            .entry("_console_token")
            .or_insert_with(|| Value::String(token.to_owned()));
        let address = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));
        let stream = LocalStream::connect_timeout(&address, timeout)
            .map_err(|_| "Cannot connect to local console")?;
        (stream, request)
    };
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|_| "Cannot configure console timeout")?;
    stream
        .set_write_timeout(Some(timeout))
        .map_err(|_| "Cannot configure console timeout")?;
    let mut bytes =
        serde_json::to_vec(&request).map_err(|_| "Invalid console request".to_owned())?;
    bytes.push(b'\n');
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err("Source request exceeds size limit".into());
    }
    stream
        .write_all(&bytes)
        .map_err(|_| "Cannot send local console request".to_owned())?;
    stream
        .shutdown(Shutdown::Write)
        .map_err(|_| "Cannot finish local console request")?;
    let mut response = Vec::with_capacity(4096);
    let mut chunk = [0u8; READ_SLICE];
    loop {
        let count = stream
            .read(&mut chunk)
            .map_err(|_| "Cannot read local console response".to_owned())?;
        if count == 0 {
            break;
        }
        if response.len().saturating_add(count) > MAX_RESPONSE_BYTES {
            return Err("Console response exceeds size limit".into());
        }
        response.extend_from_slice(&chunk[..count]);
        if response.contains(&b'\n') {
            break;
        }
    }
    let end = response
        .iter()
        .position(|b| *b == b'\n')
        .ok_or("Client closed the console without a result")?;
    serde_json::from_slice(&response[..end]).map_err(|_| "Invalid console response".into())
}

#[cfg(not(any(unix, windows)))]
pub fn send_request(
    _path: impl AsRef<Path>,
    _request: Value,
    _timeout: Duration,
) -> Result<Value, String> {
    Err("Safe local script consoles are unsupported on this platform".into())
}

#[cfg(not(any(unix, windows)))]
pub fn send_request_with_token(
    _path: impl AsRef<Path>,
    _request: Value,
    _token: &str,
    _timeout: Duration,
) -> Result<Value, String> {
    Err("Safe local script consoles are unsupported on this platform".into())
}

#[cfg(any(unix, windows))]
#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::thread;

    fn temp_dir() -> PathBuf {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "rove-console-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        #[cfg(unix)]
        {
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        #[cfg(windows)]
        drop(crate::windows_private::create_directory(&path).unwrap());
        path
    }

    #[test]
    fn lease_is_private_exclusive_and_released_on_drop() {
        let root = temp_dir();
        #[cfg(unix)]
        {
            fs::create_dir(root.join("secrets")).unwrap();
            fs::set_permissions(root.join("secrets"), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let first = SessionLease::acquire(&root).unwrap();
        assert!(SessionLease::acquire(&root).is_err());
        #[cfg(unix)]
        {
            let metadata = fs::metadata(root.join("secrets/client-session.lock")).unwrap();
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        }
        drop(first);
        assert!(SessionLease::acquire(&root).is_ok());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn socket_is_private_and_active_endpoint_is_never_replaced() {
        let root = temp_dir();
        let path = root.join("client.sock");
        let server = ScriptConsole::bind(&path, None).unwrap();
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(ScriptConsole::bind(&path, None).is_err());
        assert!(path.exists());
        drop(server);
        assert!(!path.exists());
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn one_request_roundtrip_preserves_source_bytes() {
        let root = temp_dir();
        let path = root.join("client.sock");
        let mut server = ScriptConsole::bind(&path, None).unwrap();
        let server_path = path.clone();
        let thread = thread::spawn(move || {
            let mut seen = None;
            for _ in 0..1000 {
                let dispatched = server
                    .poll(&mut |value| {
                        seen = value
                            .get("source")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        Ok(json!({"ok":true,"values":[]}))
                    })
                    .unwrap();
                if dispatched {
                    assert!(
                        server.has_pending_peer(),
                        "reply is still queued after dispatch"
                    );
                }
                if server.requests_handled() == 1 && !server.has_pending_peer() {
                    break;
                }
                thread::sleep(Duration::from_millis(1));
            }
            seen
        });
        let source = "local x = 'héllo'\nreturn x";
        let response = send_request(
            &server_path,
            json!({"source":source,"timeout_ms":1000}),
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(response["ok"], true);
        assert_eq!(thread.join().unwrap().as_deref(), Some(source));
        fs::remove_dir(root).unwrap();
    }

    #[test]
    fn authenticated_console_rejects_bad_token_before_dispatch() {
        let root = temp_dir();
        let path = root.join("client.sock");
        let mut server = ScriptConsole::bind(&path, Some("e".repeat(64))).unwrap();
        let server_thread = thread::spawn(move || {
            for _ in 0..1000 {
                let _ = server.poll(&mut |_| panic!("unauthenticated request dispatched"));
                thread::sleep(Duration::from_millis(1));
            }
        });
        let denied = send_request_with_token(
            &path,
            json!({"source":"return 1","timeout_ms":5}),
            "wrong",
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(denied["ok"], false);
        server_thread.join().unwrap();
        fs::remove_dir(root).unwrap();
    }
}
