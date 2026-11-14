//! Ordinary authenticated HTTPS session and join allocation.
//!
//! This module only carries server-issued values. It does not implement game
//! protocol authentication challenges or synthesize transport credentials.

use base64::Engine;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    fmt,
    io::Read,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::Duration,
};

const MAX_COOKIE: usize = 32 * 1024;
const MAX_REQUEST: usize = 1024 * 1024;
const MAX_RESPONSE: usize = 2 * 1024 * 1024;
const MAX_CSRF_TOKEN: usize = 8 * 1024;
const MAX_CERT_PINS: usize = 16;
const ALLOWED_HOSTS: &[&str] = &[
    "users.roblox.com",
    "games.roblox.com",
    "gamejoin.roblox.com",
    "apis.roblox.com",
    "friends.roblox.com",
    "presence.roblox.com",
    "thumbnails.roblox.com",
    "avatar.roblox.com",
    "inventory.roblox.com",
    "privatemessages.roblox.com",
    "trades.roblox.com",
    "groups.roblox.com",
    "catalog.roblox.com",
    "economy.roblox.com",
];

#[derive(Clone, PartialEq, Eq)]
struct Secret(String);

impl Secret {
    fn expose(&self) -> &str {
        &self.0
    }
}
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionError(&'static str);
impl SessionError {
    pub(crate) fn new(message: &'static str) -> Self {
        Self(message)
    }
}
impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for SessionError {}

#[derive(Clone)]
struct WireResponse {
    status: u16,
    csrf: Option<String>,
    challenge: bool,
    content_type: String,
    body: Vec<u8>,
}

trait HttpTransport: Send + Sync {
    fn send(
        &self,
        host: &str,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
    ) -> Result<WireResponse, SessionError>;
}

struct UreqTransport(ureq::Agent);
impl UreqTransport {
    fn new() -> Self {
        Self(
            ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(20))
                .redirects(0)
                .build(),
        )
    }
}
impl HttpTransport for UreqTransport {
    fn send(
        &self,
        host: &str,
        method: &str,
        path: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
    ) -> Result<WireResponse, SessionError> {
        let url = url::Url::parse(&format!("https://{host}{path}"))
            .map_err(|_| SessionError::new("Request destination is not allowed"))?;
        if url.scheme() != "https"
            || url.host_str() != Some(host)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(SessionError::new("Request destination is not allowed"));
        }
        let mut request = match method {
            "GET" => self.0.get(url.as_str()),
            "POST" => self.0.post(url.as_str()),
            "DELETE" => self.0.delete(url.as_str()),
            _ => return Err(SessionError::new("Unsupported method")),
        };
        for (key, value) in headers {
            request = request.set(key, value);
        }
        let response = match body {
            Some(bytes) => request.send_bytes(bytes),
            None => request.call(),
        };
        let response = match response {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r,
            Err(_) => return Err(SessionError::new("HTTPS request failed")),
        };
        let status = response.status();
        let csrf = response.header("x-csrf-token").map(str::to_owned);
        let challenge = response.header("rblx-challenge-id").is_some();
        let content_type = response
            .header("content-type")
            .unwrap_or("")
            .chars()
            .take(256)
            .collect();
        let mut body = Vec::new();
        response
            .into_reader()
            .take((MAX_RESPONSE + 1) as u64)
            .read_to_end(&mut body)
            .map_err(|_| SessionError::new("HTTPS response could not be read"))?;
        Ok(WireResponse {
            status,
            csrf,
            challenge,
            content_type,
            body,
        })
    }
}

/// An authenticated session. Its cookie and cached anti-CSRF tokens are never
/// included in Debug output; callers should construct it from a secret store.
pub struct RobloxSession {
    cookie: Secret,
    csrf: HashMap<String, Secret>,
    transport: Box<dyn HttpTransport>,
}
impl fmt::Debug for RobloxSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RobloxSession")
            .field("cookie", &"[REDACTED]")
            .field("csrf_hosts", &self.csrf.len())
            .finish_non_exhaustive()
    }
}
impl RobloxSession {
    pub fn new(cookie: &str) -> Result<Self, SessionError> {
        Self::with_transport(cookie, Box::new(UreqTransport::new()))
    }
    fn with_transport(
        cookie: &str,
        transport: Box<dyn HttpTransport>,
    ) -> Result<Self, SessionError> {
        if cookie.len() > MAX_COOKIE {
            return Err(SessionError::new("Invalid account session"));
        }
        let mut cookie = cookie.trim();
        if let Some(value) = cookie.strip_prefix(".ROBLOSECURITY=") {
            cookie = value.trim_end_matches(';');
        }
        if cookie.is_empty() || !cookie.bytes().all(|b| (33..=126).contains(&b) && b != b';') {
            return Err(SessionError::new(
                "Cookie must contain only the cookie value",
            ));
        }
        Ok(Self {
            cookie: Secret(cookie.to_owned()),
            csrf: HashMap::new(),
            transport,
        })
    }

    pub fn request(
        &mut self,
        host: &str,
        path: &str,
        method: &str,
        payload: Option<&Value>,
        csrf_retry: bool,
    ) -> Result<HttpReply, SessionError> {
        if !ALLOWED_HOSTS.contains(&host) || !valid_path(path) {
            return Err(SessionError::new("Request destination is not allowed"));
        }
        if method != "GET"
            && method != "POST"
            && !(method == "DELETE" && host == "groups.roblox.com")
        {
            return Err(SessionError::new("Unsupported method"));
        }
        let body = payload
            .map(serde_json::to_vec)
            .transpose()
            .map_err(|_| SessionError::new("Request payload could not be encoded"))?;
        if body.as_ref().is_some_and(|body| body.len() > MAX_REQUEST) {
            return Err(SessionError::new("Request payload exceeded the size limit"));
        }
        for attempt in 0..if csrf_retry { 2 } else { 1 } {
            let mut headers = vec![
                (
                    "Cookie".into(),
                    format!(".ROBLOSECURITY={}", self.cookie.expose()),
                ),
                ("Accept".into(), "application/json".into()),
                ("User-Agent".into(), "LocalRobloxResearch/1.0".into()),
                ("Origin".into(), "https://www.roblox.com".into()),
                ("Referer".into(), "https://www.roblox.com/".into()),
            ];
            if body.is_some() {
                headers.push(("Content-Type".into(), "application/json".into()));
            }
            if let Some(token) = self.csrf.get(host) {
                headers.push(("X-CSRF-TOKEN".into(), token.expose().to_owned()));
            }
            let wire = self
                .transport
                .send(host, method, path, &headers, body.as_deref())?;
            if wire.body.len() > MAX_RESPONSE {
                return Err(SessionError::new("Response exceeded the size limit"));
            }
            if wire.status == 403 && attempt == 0 && csrf_retry && !wire.challenge {
                if let Some(token) = wire.csrf.as_ref().filter(|v| valid_csrf_token(v)) {
                    self.csrf.insert(host.to_owned(), Secret(token.clone()));
                    continue;
                }
            }
            let data = serde_json::from_slice(&wire.body).ok();
            return Ok(HttpReply {
                http_status: wire.status,
                challenge_present: wire.challenge,
                content_type: wire.content_type,
                data,
            });
        }
        Err(SessionError::new("Request retry was exhausted"))
    }
}

fn valid_path(path: &str) -> bool {
    if !path.starts_with('/')
        || path.contains('\\')
        || path.contains('#')
        || path.bytes().any(|b| b < 32 || b == 127 || b == b' ')
    {
        return false;
    }
    let bytes = path.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 >= bytes.len()
                || !bytes[i + 1].is_ascii_hexdigit()
                || !bytes[i + 2].is_ascii_hexdigit()
            {
                return false;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    let (raw_path, raw_query) = path
        .split_once('?')
        .map(|(p, q)| (p, Some(q)))
        .unwrap_or((path, None));
    let Ok(url) = url::Url::parse(&format!("https://gamejoin.roblox.com{path}")) else {
        return false;
    };
    url.scheme() == "https"
        && url.host_str() == Some("gamejoin.roblox.com")
        && url.path() == raw_path
        && url.query() == raw_query
        && url.fragment().is_none()
}

fn valid_csrf_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= MAX_CSRF_TOKEN
        && token.bytes().all(|b| (33..=126).contains(&b))
}

pub struct HttpReply {
    pub http_status: u16,
    pub challenge_present: bool,
    pub content_type: String,
    pub data: Option<Value>,
}
impl fmt::Debug for HttpReply {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpReply")
            .field("http_status", &self.http_status)
            .field("challenge_present", &self.challenge_present)
            .field("content_type_length", &self.content_type.len())
            .field(
                "data",
                &self.data.as_ref().map(|_| "[REDACTED RESPONSE BODY]"),
            )
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    AltOwnedPublicPlace,
    UserSelectedPlace,
    UserSelectedPrivateServer,
}
#[derive(Clone, Debug)]
pub struct JoinTarget {
    pub place_id: u64,
    pub is_playable: bool,
    pub selection: Selection,
    pub job_id: Option<String>,
}
#[derive(Clone)]
pub struct PrivateInvite {
    pub place_id: u64,
    pub owner_user_id: u64,
    pub private_server_id: u64,
    credential: Secret,
    credential_kind: PrivateInviteKind,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum PrivateInviteKind {
    LinkCode,
    OwnedAccessCode,
}
impl PrivateInvite {
    pub fn new(
        place_id: u64,
        owner_user_id: u64,
        private_server_id: u64,
        link_code: &str,
    ) -> Result<Self, SessionError> {
        if link_code.is_empty()
            || link_code.len() > 4096
            || link_code.bytes().any(|b| b <= 32 || b == 127)
        {
            return Err(SessionError::new("Invalid private invite"));
        }
        Ok(Self {
            place_id,
            owner_user_id,
            private_server_id,
            credential: Secret(link_code.to_owned()),
            credential_kind: PrivateInviteKind::LinkCode,
        })
    }

    pub fn new_owned_access_code(
        place_id: u64,
        owner_user_id: u64,
        private_server_id: u64,
        access_code: &str,
    ) -> Result<Self, SessionError> {
        let mut invite = Self::new(place_id, owner_user_id, private_server_id, access_code)?;
        invite.credential_kind = PrivateInviteKind::OwnedAccessCode;
        Ok(invite)
    }
}
impl fmt::Debug for PrivateInvite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PrivateInvite")
            .field("place_id", &self.place_id)
            .field("owner_user_id", &self.owner_user_id)
            .field("private_server_id", &self.private_server_id)
            .field("credential", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct SessionTicketMetadata {
    client_ticket: Option<Secret>,
    join_ticket: Option<Secret>,
}
impl fmt::Debug for SessionTicketMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionTicketMetadata")
            .field(
                "client_ticket",
                &self.client_ticket.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "join_ticket",
                &self.join_ticket.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}
impl SessionTicketMetadata {
    pub fn client_ticket(&self) -> Option<&str> {
        self.client_ticket.as_ref().map(Secret::expose)
    }
    pub fn join_ticket(&self) -> Option<&str> {
        self.join_ticket.as_ref().map(Secret::expose)
    }
}

/// Explicit allocation fields parsed only from the HTTPS server's issued
/// `joinScript`. Fields required for native transport can be handed to the
/// transport constructor once all validation succeeds.
#[derive(Clone, PartialEq, Eq)]
pub struct IssuedJoinAllocation {
    pub place_id: u64,
    /// User identity and session id issued by the allocation response.
    /// `user_id` is checked against the authenticated account before return.
    pub user_id: u64,
    pub user_name: String,
    pub display_name: String,
    pub has_verified_badge: bool,
    session_id: Secret,
    pub endpoint: Option<SocketAddr>,
    pub backend: Option<Ipv4Addr>,
    token: Option<[u8; 16]>,
    pub certificate_sha256: Vec<[u8; 32]>,
    /// Local feature offer; the allocation-only path enables no optional bits.
    pub capabilities: u64,
    pub game_id: Option<String>,
    /// Numeric VIP-management id selected from the authenticated account list.
    pub requested_vip_server_id: Option<u64>,
    /// Routing id actually issued in the join response; it may be a UUID and
    /// is deliberately distinct from the numeric VIP-management id.
    private_server_routing_id: Option<Secret>,
    pub private_server_owner_id: Option<u64>,
    pub tickets: SessionTicketMetadata,
    // Kept private and omitted from Debug. These exact fields belong to this
    // allocation; a live owner must not reconstruct them from another join.
    game_join_fields: Value,
    game_join_ticket: Option<Value>,
}
impl IssuedJoinAllocation {
    pub fn session_id(&self) -> &str {
        self.session_id.expose()
    }
    pub fn token(&self) -> Option<&[u8; 16]> {
        self.token.as_ref()
    }
    pub fn private_server_routing_id(&self) -> Option<&str> {
        self.private_server_routing_id.as_ref().map(Secret::expose)
    }
    pub fn is_transport_ready(&self) -> bool {
        self.endpoint.is_some()
            && self.backend.is_some()
            && self.token.is_some()
            && !self.certificate_sha256.is_empty()
    }
    /// Prepare ordered login messages from this allocation only. Wire bytes
    /// contain credentials and intentionally have no logging/Debug wrapper.
    pub fn game_login_packets(
        &self,
        range: Option<(u8, u8)>,
    ) -> Result<Vec<Vec<u8>>, SessionError> {
        let ticket = self
            .tickets
            .client_ticket()
            .ok_or_else(|| SessionError::new("Issued client ticket missing"))?;
        let encode = || -> Result<Vec<Vec<u8>>, crate::login::LoginCodecError> {
            Ok(vec![
                crate::login::issued_early_auth(ticket)?,
                crate::login::protocol_sync(
                    &self.game_join_fields,
                    self.game_join_ticket.as_ref(),
                    range,
                )?,
                crate::login::verification_packet()?,
                crate::login::ticket_packet(&self.game_join_fields)?,
                crate::login::server_id_packet(),
            ])
        };
        encode().map_err(|_| SessionError::new("Issued game login fields could not be encoded"))
    }
}
impl fmt::Debug for IssuedJoinAllocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssuedJoinAllocation")
            .field("place_id", &self.place_id)
            .field("user_id", &"[VERIFIED]")
            .field("user_name", &"[REDACTED]")
            .field("display_name", &"[REDACTED]")
            .field("has_verified_badge", &self.has_verified_badge)
            .field("session_id", &"[REDACTED]")
            .field("endpoint", &self.endpoint)
            .field("backend", &self.backend)
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .field("certificate_pin_count", &self.certificate_sha256.len())
            .field("capabilities", &self.capabilities)
            .field("game_id", &self.game_id)
            .field("requested_vip_server_id", &self.requested_vip_server_id)
            .field(
                "private_server_routing_id",
                &self
                    .private_server_routing_id
                    .as_ref()
                    .map(|_| "[REDACTED]"),
            )
            .field("private_server_owner_id", &self.private_server_owner_id)
            .field("tickets", &self.tickets)
            .finish()
    }
}

#[derive(Debug)]
pub enum AllocationResult {
    Pending {
        status: i64,
        http_status: u16,
    },
    Challenge {
        http_status: u16,
    },
    Rejected {
        http_status: u16,
        status: Option<i64>,
    },
    Ready(IssuedJoinAllocation),
}

/// Performs the normal join-allocation exchange; caller owns any retry delay.
pub fn request_allocation(
    session: &mut RobloxSession,
    target: &JoinTarget,
    invite: Option<&PrivateInvite>,
    attempt_id: &str,
    expected_user_id: u64,
) -> Result<AllocationResult, SessionError> {
    if !target.is_playable {
        return Err(SessionError::new("Target is not verified playable"));
    }
    if attempt_id.len() != 36 || !canonical_uuid(attempt_id) {
        return Err(SessionError::new("Invalid join attempt identifier"));
    }
    let private = target.selection == Selection::UserSelectedPrivateServer;
    if private && invite.map(|i| i.place_id) != Some(target.place_id) {
        return Err(SessionError::new(
            "Private invite does not match selected place",
        ));
    }
    if !private && invite.is_some() {
        return Err(SessionError::new("Unexpected private invite"));
    }
    let mut payload =
        json!({"placeId":target.place_id,"gameJoinAttemptId":attempt_id,"isTeleport":false});
    let endpoint = if private {
        "/v1/join-private-game"
    } else {
        "/v1/join-game"
    };
    if private {
        let invite = invite.expect("validated");
        match invite.credential_kind {
            PrivateInviteKind::LinkCode => payload["linkCode"] = json!(invite.credential.expose()),
            PrivateInviteKind::OwnedAccessCode => {
                payload["accessCode"] = json!(invite.credential.expose())
            }
        }
    }
    let endpoint = if let Some(job) = target.job_id.as_deref() {
        if private || !canonical_uuid(job) {
            return Err(SessionError::new("Invalid public server selection"));
        }
        payload["gameId"] = json!(job);
        "/v1/join-game-instance"
    } else {
        endpoint
    };
    let reply = session.request(
        "gamejoin.roblox.com",
        endpoint,
        "POST",
        Some(&payload),
        true,
    )?;
    if reply.challenge_present {
        return Ok(AllocationResult::Challenge {
            http_status: reply.http_status,
        });
    }
    let data = reply.data.as_ref().and_then(Value::as_object);
    let status = data.and_then(|v| v.get("status")).and_then(Value::as_i64);
    if reply.http_status == 200
        && matches!(status, Some(0 | 1))
        && data
            .and_then(|d| d.get("joinScript"))
            .map_or(true, Value::is_null)
        && data
            .and_then(|d| d.get("joinScriptUrl"))
            .map_or(true, Value::is_null)
    {
        return Ok(AllocationResult::Pending {
            status: status.unwrap(),
            http_status: reply.http_status,
        });
    }
    if reply.http_status != 200 || status != Some(2) {
        return Ok(AllocationResult::Rejected {
            http_status: reply.http_status,
            status,
        });
    }
    let join = data
        .and_then(|d| d.get("joinScript"))
        .and_then(Value::as_object)
        .ok_or(SessionError::new("Ready allocation omitted join script"))?;
    let user_id = join
        .get("UserId")
        .and_then(value_u64)
        .ok_or(SessionError::new(
            "Issued allocation omitted account identity",
        ))?;
    if expected_user_id == 0 || user_id != expected_user_id {
        return Err(SessionError::new(
            "Issued allocation did not match authenticated account",
        ));
    }
    let session_id = secret_field(join, "SessionId").ok_or(SessionError::new(
        "Issued allocation omitted session identity",
    ))?;
    let user_name = bounded_issued_text(join, "UserName")
        .ok_or(SessionError::new("Issued allocation omitted player name"))?;
    let display_name =
        bounded_issued_text(join, "DisplayName").unwrap_or_else(|| user_name.clone());
    if join.get("PlaceId").and_then(value_u64) != Some(target.place_id) {
        return Err(SessionError::new(
            "Issued allocation did not match requested place",
        ));
    }
    if let Some(job) = target.job_id.as_deref() {
        let actual = join
            .get("GameId")
            .and_then(Value::as_str)
            .ok_or(SessionError::new(
                "Issued allocation did not match requested server",
            ))?;
        if !canonical_uuid(actual) || !actual.eq_ignore_ascii_case(job) {
            return Err(SessionError::new(
                "Issued allocation did not match requested server",
            ));
        }
    }
    let routing_id = join.get("PrivateServerID");
    let (private_server_routing_id, requested_vip_server_id) =
        parse_private_routing_id(routing_id, invite)?;
    let owner_id = join.get("PrivateServerOwnerID").and_then(value_u64);
    if let Some(invite) = invite {
        if owner_id != Some(invite.owner_user_id)
            || join.get("PlaceId").and_then(value_u64) != Some(target.place_id)
        {
            return Err(SessionError::new(
                "Issued allocation did not match requested private server",
            ));
        }
    }
    let endpoint_value = parse_endpoint(join);
    let backend = join
        .get("MachineAddress")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<Ipv4Addr>().ok())
        .filter(|ip| ip.is_private());
    let token = join
        .get("NetStackTokenValue")
        .and_then(Value::as_str)
        .and_then(|s| base64::engine::general_purpose::STANDARD.decode(s).ok())
        .and_then(|v| v.try_into().ok());
    let (pins, port) = parse_net_stack(join);
    let endpoint = endpoint_value
        .and_then(|ip| port.and_then(|port| SocketAddr::new(IpAddr::V4(ip), port).into()));
    let tickets = SessionTicketMetadata {
        client_ticket: secret_field(join, "ClientTicket"),
        join_ticket: data
            .and_then(|d| d.get("joinTicket"))
            .and_then(Value::as_str)
            .map(|v| Secret(v.to_owned())),
    };
    Ok(AllocationResult::Ready(IssuedJoinAllocation {
        place_id: target.place_id,
        user_id,
        user_name,
        display_name,
        has_verified_badge: join
            .get("HasVerifiedBadge")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        session_id,
        endpoint,
        backend,
        token,
        certificate_sha256: pins,
        capabilities: 0,
        game_id: join
            .get("GameId")
            .and_then(Value::as_str)
            .map(str::to_owned),
        requested_vip_server_id,
        private_server_routing_id,
        private_server_owner_id: owner_id,
        tickets,
        game_join_fields: crate::login::retain_issued_game_fields(join),
        game_join_ticket: data.and_then(|d| d.get("joinTicket")).cloned(),
    }))
}

fn parse_private_routing_id(
    wire_value: Option<&Value>,
    invite: Option<&PrivateInvite>,
) -> Result<(Option<Secret>, Option<u64>), SessionError> {
    let Some(value) = wire_value.filter(|value| !value.is_null()) else {
        if invite.is_some() {
            return Err(SessionError::new(
                "Issued allocation omitted private server routing id",
            ));
        }
        return Ok((None, None));
    };
    if let Some(text) = value.as_str() {
        if text.is_empty() && invite.is_none() {
            return Ok((None, None));
        }
        if !canonical_non_nil_uuid(text) {
            return Err(SessionError::new(
                "Issued private server routing id had an unsupported format",
            ));
        }
        return Ok((
            Some(Secret(text.to_owned())),
            invite.map(|i| i.private_server_id),
        ));
    }
    if value.as_u64() == Some(0) && invite.is_none() {
        return Ok((None, None));
    }
    if let Some(numeric_id) = value.as_u64().filter(|id| *id > 0) {
        match invite {
            Some(invite)
                if invite.credential_kind == PrivateInviteKind::LinkCode
                    && numeric_id == invite.private_server_id =>
            {
                return Ok((Some(Secret(numeric_id.to_string())), Some(numeric_id)));
            }
            Some(_) => {
                return Err(SessionError::new(
                    "Issued private server routing id did not match the selected server",
                ));
            }
            None => return Ok((Some(Secret(numeric_id.to_string())), None)),
        }
    }
    Err(SessionError::new(
        "Issued private server routing id had an unsupported format",
    ))
}

fn canonical_non_nil_uuid(value: &str) -> bool {
    if value.len() != 36 {
        return false;
    }
    let mut nonzero = false;
    for (index, byte) in value.bytes().enumerate() {
        if [8, 13, 18, 23].contains(&index) {
            if byte != b'-' {
                return false;
            }
        } else if byte.is_ascii_hexdigit() {
            nonzero |= byte != b'0';
            if index == 14 && !matches!(byte.to_ascii_lowercase(), b'1'..=b'8') {
                return false;
            }
            if index == 19 && !matches!(byte.to_ascii_lowercase(), b'8' | b'9' | b'a' | b'b') {
                return false;
            }
        } else {
            return false;
        }
    }
    nonzero
}

/// Matches the existing pending-allocation policy: at most eight POSTs, three
/// seconds apart, and only while the server explicitly returns status 0/1
/// without a join script. Challenges and all other replies stop immediately.
pub fn request_allocation_until_ready(
    session: &mut RobloxSession,
    target: &JoinTarget,
    invite: Option<&PrivateInvite>,
    attempt_id: &str,
    expected_user_id: u64,
) -> Result<AllocationResult, SessionError> {
    for attempt in 0..8 {
        let result = request_allocation(session, target, invite, attempt_id, expected_user_id)?;
        if matches!(result, AllocationResult::Pending { .. }) && attempt < 7 {
            std::thread::sleep(Duration::from_secs(3));
            continue;
        }
        return Ok(result);
    }
    Err(SessionError::new("Allocation retry was exhausted"))
}

fn value_u64(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str()?.parse().ok())
}
fn secret_field(obj: &serde_json::Map<String, Value>, key: &str) -> Option<Secret> {
    obj.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(|s| Secret(s.to_owned()))
}

fn bounded_issued_text(obj: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    let value = obj.get(key).and_then(Value::as_str)?;
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return None;
    }
    Some(value.to_owned())
}

#[cfg(test)]
pub(crate) fn test_issued_allocation() -> IssuedJoinAllocation {
    IssuedJoinAllocation {
        place_id: 123,
        user_id: 7,
        user_name: "tester".into(),
        display_name: "Tester".into(),
        has_verified_badge: false,
        session_id: Secret("sensitive-session".into()),
        endpoint: Some("192.0.2.1:443".parse().unwrap()),
        backend: Some("10.0.0.1".parse().unwrap()),
        token: Some([7; 16]),
        certificate_sha256: vec![[3; 32]],
        capabilities: 0,
        game_id: None,
        requested_vip_server_id: None,
        private_server_routing_id: None,
        private_server_owner_id: None,
        tickets: SessionTicketMetadata {
            client_ticket: None,
            join_ticket: None,
        },
        game_join_fields: json!({}),
        game_join_ticket: None,
    }
}
fn parse_endpoint(join: &serde_json::Map<String, Value>) -> Option<Ipv4Addr> {
    let endpoints = join.get("UdmuxEndpoints")?.as_array()?;
    let first = endpoints.first()?.as_object()?;
    let addr = first
        .get("Address")
        .or_else(|| first.get("address"))?
        .as_str()?
        .parse::<Ipv4Addr>()
        .ok()?;
    if is_global_ipv4(addr) {
        Some(addr)
    } else {
        None
    }
}
fn parse_net_stack(join: &serde_json::Map<String, Value>) -> (Vec<[u8; 32]>, Option<u16>) {
    let Some(encoded) = join.get("NetStackConfig").and_then(Value::as_str) else {
        return (vec![], None);
    };
    let Some(decoded) = percent_decode(encoded) else {
        return (vec![], None);
    };
    let Ok(config) = serde_json::from_str::<Value>(&decoded) else {
        return (vec![], None);
    };
    let port = config
        .get("port")
        .and_then(value_u64)
        .and_then(|p| u16::try_from(p).ok())
        .filter(|p| *p != 0);
    if port
        != join
            .get("NetStackPort")
            .and_then(value_u64)
            .and_then(|p| u16::try_from(p).ok())
    {
        return (vec![], None);
    }
    let Some(values) = config.get("certHashes").and_then(Value::as_array) else {
        return (vec![], port);
    };
    if values.is_empty() || values.len() > MAX_CERT_PINS {
        return (vec![], port);
    }
    let pins: Option<Vec<[u8; 32]>> = values
        .iter()
        .map(|v| {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(v.as_str()?)
                .ok()?;
            decoded.try_into().ok()
        })
        .collect();
    (pins.unwrap_or_default(), port)
}

fn is_global_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, d] = ip.octets();
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip == Ipv4Addr::BROADCAST
        || a == 0
        || a == 100 && (64..=127).contains(&b) // shared address space
        || a == 192 && b == 0 && c == 0
        || a == 192 && b == 0 && c == 2
        || a == 192 && b == 88 && c == 99
        || a == 198 && (b == 18 || b == 19 || b == 51 && c == 100)
        || a == 203 && b == 0 && c == 113
        || a >= 240
        || d == 255 && a == 255 && b == 255 && c == 255)
}
fn percent_decode(s: &str) -> Option<String> {
    let mut out = Vec::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            if i + 2 >= b.len() {
                return None;
            }
            let hi = (b[i + 1] as char).to_digit(16)? as u8;
            let lo = (b[i + 2] as char).to_digit(16)? as u8;
            out.push((hi << 4) | lo);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}
fn canonical_uuid(value: &str) -> bool {
    let mut nonzero = false;
    value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => {
                let valid = b.is_ascii_hexdigit();
                nonzero |= valid && b != b'0';
                valid
            }
        })
        && nonzero
}

pub fn new_join_attempt_id() -> Result<String, SessionError> {
    let mut bytes = [0u8; 16];
    rove_client_core::entropy::fill(&mut bytes)
        .map_err(|_| SessionError::new("Could not generate join attempt identifier"))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(format!("{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}", bytes[0],bytes[1],bytes[2],bytes[3],bytes[4],bytes[5],bytes[6],bytes[7],bytes[8],bytes[9],bytes[10],bytes[11],bytes[12],bytes[13],bytes[14],bytes[15]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    struct Mock {
        replies: Mutex<Vec<WireResponse>>,
        requests: Mutex<Vec<(String, String, String, Vec<(String, String)>)>>,
        bodies: Mutex<Vec<Option<Vec<u8>>>>,
    }
    impl HttpTransport for Mock {
        fn send(
            &self,
            h: &str,
            m: &str,
            p: &str,
            headers: &[(String, String)],
            body: Option<&[u8]>,
        ) -> Result<WireResponse, SessionError> {
            self.requests
                .lock()
                .unwrap()
                .push((h.into(), m.into(), p.into(), headers.to_vec()));
            self.bodies.lock().unwrap().push(body.map(<[u8]>::to_vec));
            if self.replies.lock().unwrap().is_empty() {
                return Err(SessionError::new("no fixture response"));
            }
            Ok(self.replies.lock().unwrap().remove(0))
        }
    }
    fn wire(status: u16, csrf: Option<&str>, challenge: bool, body: &str) -> WireResponse {
        WireResponse {
            status,
            csrf: csrf.map(str::to_owned),
            challenge,
            content_type: "application/json".into(),
            body: body.as_bytes().to_vec(),
        }
    }
    fn ready_body(place_id: u64) -> Value {
        json!({"status":2,"joinScript":{"PlaceId":place_id,"UserId":42,"UserName":"tester","DisplayName":"Tester","HasVerifiedBadge":false,"SessionId":"session-secret","GameId":"00000000-0000-4000-8000-000000000002","ClientTicket":"ticket-secret","PrivateServerID":77,"PrivateServerOwnerID":88,"MachineAddress":"10.0.0.5","UdmuxEndpoints":[{"Address":"8.8.8.8"}],"NetStackPort":443,"NetStackTokenValue":base64::engine::general_purpose::STANDARD.encode([7u8;16]),"NetStackConfig":format!("%7B%22port%22%3A443%2C%22certHashes%22%3A[%22{}%22]%7D",base64::engine::general_purpose::STANDARD.encode([9u8;32]))}})
    }
    fn target(place_id: u64) -> JoinTarget {
        JoinTarget {
            place_id,
            is_playable: true,
            selection: Selection::UserSelectedPlace,
            job_id: None,
        }
    }
    fn mock_session(responses: Vec<WireResponse>) -> (RobloxSession, std::sync::Arc<Mock>) {
        let m = std::sync::Arc::new(Mock {
            replies: Mutex::new(responses),
            requests: Mutex::new(vec![]),
            bodies: Mutex::new(vec![]),
        });
        let boxed: Box<dyn HttpTransport> = Box::new(ArcMock(m.clone()));
        (
            RobloxSession::with_transport("secret-cookie", boxed).unwrap(),
            m,
        )
    }
    struct ArcMock(std::sync::Arc<Mock>);
    impl HttpTransport for ArcMock {
        fn send(
            &self,
            h: &str,
            m: &str,
            p: &str,
            heads: &[(String, String)],
            b: Option<&[u8]>,
        ) -> Result<WireResponse, SessionError> {
            self.0.send(h, m, p, heads, b)
        }
    }
    #[test]
    fn fresh_allocation_preserves_object_join_ticket_for_rust_login() {
        let ticket = format!(
            "a;b;{};{};1",
            base64::engine::general_purpose::STANDARD.encode([1; 32]),
            base64::engine::general_purpose::STANDARD.encode([2; 16])
        );
        let mut body = ready_body(123);
        body["joinScript"]["ClientTicket"] = json!(ticket);
        body["joinTicket"] = json!({"owned_fixture":"issued-join-ticket"});
        let (mut session, _) = mock_session(vec![wire(200, None, false, &body.to_string())]);
        let result = request_allocation(
            &mut session,
            &target(123),
            None,
            "00000000-0000-4000-8000-000000000001",
            42,
        )
        .unwrap();
        let AllocationResult::Ready(issued) = result else {
            panic!("expected fixture allocation")
        };
        let packets = issued.game_login_packets(Some((13, 14))).unwrap();
        assert_eq!(
            packets.iter().map(|p| p[0]).collect::<Vec<_>>(),
            vec![168, 144, 146, 138, 143]
        );
        assert_eq!(
            packets[1],
            crate::login::protocol_sync(
                &body["joinScript"],
                Some(&body["joinTicket"]),
                Some((13, 14))
            )
            .unwrap()
        );
        assert_eq!(
            packets[3],
            crate::login::ticket_packet(&body["joinScript"]).unwrap()
        );
        let debug = format!("{issued:?}");
        for secret in [ticket.as_str(), "issued-join-ticket", "session-secret"] {
            assert!(!debug.contains(secret));
        }
        assert!(test_issued_allocation().game_login_packets(None).is_err());
    }

    #[test]
    fn csrf_retries_once_and_caches_server_token() {
        let (mut s, m) = mock_session(vec![
            wire(403, Some("server-csrf"), false, "{}"),
            wire(200, None, false, "{\"ok\":true}"),
        ]);
        let r = s
            .request(
                "gamejoin.roblox.com",
                "/v1/join-game",
                "POST",
                Some(&json!({})),
                true,
            )
            .unwrap();
        assert_eq!(r.http_status, 200);
        let req = m.requests.lock().unwrap();
        assert_eq!(req.len(), 2);
        assert!(req[0]
            .3
            .iter()
            .any(|(k, v)| k == "Cookie" && v.contains("secret-cookie")));
        assert!(req[1]
            .3
            .iter()
            .any(|(k, v)| k == "X-CSRF-TOKEN" && v == "server-csrf"));
        assert!(!format!("{:?}", s).contains("secret-cookie"));
    }
    #[test]
    fn challenge_prevents_csrf_retry() {
        let (mut s, m) = mock_session(vec![wire(403, Some("token"), true, "{}")]);
        let r = s
            .request("gamejoin.roblox.com", "/v1/join-game", "POST", None, true)
            .unwrap();
        assert!(r.challenge_present);
        assert_eq!(m.requests.lock().unwrap().len(), 1);
    }
    #[test]
    fn csrf_tokens_are_host_scoped_and_invalid_tokens_are_ignored() {
        let (mut s, mock) = mock_session(vec![
            wire(403, Some("gamejoin-token"), false, "{}"),
            wire(200, None, false, "{}"),
            wire(200, None, false, "{}"),
            wire(403, Some("bad\nheader"), false, "{}"),
        ]);
        assert_eq!(
            s.request("gamejoin.roblox.com", "/v1/x", "GET", None, true)
                .unwrap()
                .http_status,
            200
        );
        assert_eq!(
            s.request("games.roblox.com", "/v1/x", "GET", None, false)
                .unwrap()
                .http_status,
            200
        );
        let _ = s
            .request("games.roblox.com", "/v1/y", "GET", None, true)
            .unwrap();
        let requests = mock.requests.lock().unwrap();
        assert!(requests[2].3.iter().all(|(k, _)| k != "X-CSRF-TOKEN"));
        assert_eq!(requests.len(), 4);
        assert!(requests[3].3.iter().all(|(k, _)| k != "X-CSRF-TOKEN"));
    }
    #[test]
    fn blocks_unlisted_hosts_paths_and_methods() {
        let (mut s, _) = mock_session(vec![wire(200, None, false, "{}")]);
        assert!(s.request("evil.example", "/", "GET", None, false).is_err());
        assert!(s
            .request("gamejoin.roblox.com", "/x\ny", "GET", None, false)
            .is_err());
        assert!(s
            .request("groups.roblox.com", "/", "DELETE", None, false)
            .is_ok());
        assert!(s
            .request("gamejoin.roblox.com", "/", "DELETE", None, false)
            .is_err());
        for path in ["/x\\evil", "/x#fragment", "/x%Q0", "/x?a=%Q0", "/x\u{7f}"] {
            assert!(
                s.request("gamejoin.roblox.com", path, "GET", None, false)
                    .is_err(),
                "{path:?}"
            );
        }
    }
    #[test]
    fn request_and_response_sizes_are_bounded_before_decode_or_send() {
        let (mut s, mock) = mock_session(vec![wire(200, None, false, "{}")]);
        let huge = json!({"body":"x".repeat(MAX_REQUEST)});
        assert!(s
            .request("gamejoin.roblox.com", "/v1/x", "POST", Some(&huge), false)
            .is_err());
        assert!(mock.requests.lock().unwrap().is_empty());

        let oversized = WireResponse {
            status: 200,
            csrf: None,
            challenge: false,
            content_type: "application/json".into(),
            body: vec![b'x'; MAX_RESPONSE + 1],
        };
        let (mut s, _) = mock_session(vec![oversized]);
        assert_eq!(
            s.request("gamejoin.roblox.com", "/v1/x", "GET", None, false)
                .unwrap_err()
                .to_string(),
            "Response exceeded the size limit"
        );
    }
    #[test]
    fn pending_and_rejected_join_statuses_are_not_ready() {
        let (mut s, _) = mock_session(vec![wire(200, None, false, r#"{"status":1}"#)]);
        assert!(matches!(
            request_allocation(
                &mut s,
                &target(12),
                None,
                "00000000-0000-4000-8000-000000000001",
                42
            )
            .unwrap(),
            AllocationResult::Pending { status: 1, .. }
        ));
        let (mut s, _) = mock_session(vec![wire(401, None, false, r#"{"status":0}"#)]);
        assert!(matches!(
            request_allocation(
                &mut s,
                &target(12),
                None,
                "00000000-0000-4000-8000-000000000001",
                42
            )
            .unwrap(),
            AllocationResult::Rejected {
                http_status: 401,
                ..
            }
        ));
    }
    #[test]
    fn explicit_null_join_script_fields_are_pending() {
        for body in [
            r#"{"status":1}"#,
            r#"{"status":1,"joinScript":null}"#,
            r#"{"status":1,"joinScriptUrl":null}"#,
            r#"{"status":1,"joinScript":null,"joinScriptUrl":null}"#,
        ] {
            let (mut session, _) = mock_session(vec![wire(200, None, false, body)]);
            assert!(matches!(
                request_allocation(
                    &mut session,
                    &target(12),
                    None,
                    "00000000-0000-4000-8000-000000000001",
                    42,
                )
                .unwrap(),
                AllocationResult::Pending { status: 1, .. }
            ));
        }
        let (mut session, _) = mock_session(vec![wire(
            200,
            None,
            false,
            r#"{"status":1,"joinScript":{},"joinScriptUrl":null}"#,
        )]);
        assert!(matches!(
            request_allocation(
                &mut session,
                &target(12),
                None,
                "00000000-0000-4000-8000-000000000001",
                42,
            )
            .unwrap(),
            AllocationResult::Rejected { .. }
        ));
    }

    #[test]
    fn owned_private_server_uses_issued_access_code_instead_of_link_code() {
        let (mut session, mock) = mock_session(vec![wire(
            200,
            None,
            false,
            r#"{"status":1,"joinScript":null,"joinScriptUrl":null}"#,
        )]);
        let target = JoinTarget {
            place_id: 12,
            is_playable: true,
            selection: Selection::UserSelectedPrivateServer,
            job_id: None,
        };
        let invite =
            PrivateInvite::new_owned_access_code(12, 88, 77, "issued-access-secret").unwrap();
        let result = request_allocation(
            &mut session,
            &target,
            Some(&invite),
            "00000000-0000-4000-8000-000000000001",
            42,
        )
        .unwrap();
        assert!(matches!(result, AllocationResult::Pending { .. }));
        let bodies = mock.bodies.lock().unwrap();
        let json: Value = serde_json::from_slice(bodies[0].as_ref().unwrap()).unwrap();
        assert_eq!(json["accessCode"], "issued-access-secret");
        assert!(json.get("linkCode").is_none());
        assert!(!format!("{:?}", invite).contains("issued-access-secret"));

        let (mut session, mock) = mock_session(vec![wire(200, None, false, r#"{"status":1}"#)]);
        let link_invite = PrivateInvite::new(12, 88, 77, "issued-link-secret").unwrap();
        request_allocation(
            &mut session,
            &target,
            Some(&link_invite),
            "00000000-0000-4000-8000-000000000001",
            42,
        )
        .unwrap();
        let bodies = mock.bodies.lock().unwrap();
        let json: Value = serde_json::from_slice(bodies[0].as_ref().unwrap()).unwrap();
        assert_eq!(json["linkCode"], "issued-link-secret");
        assert!(json.get("accessCode").is_none());
        assert!(!format!("{:?}", link_invite).contains("issued-link-secret"));
    }

    #[test]
    fn owned_allocation_keeps_issued_uuid_routing_id_distinct_from_vip_id() {
        let issued_uuid = "550e8400-e29b-41d4-a716-446655440000";
        let mut body = ready_body(12);
        body["joinScript"]["PrivateServerID"] = json!(issued_uuid);
        let (mut session, _) = mock_session(vec![wire(200, None, false, &body.to_string())]);
        let mut private_target = target(12);
        private_target.selection = Selection::UserSelectedPrivateServer;
        let invite = PrivateInvite::new_owned_access_code(12, 88, 77, "fresh-access-code").unwrap();
        let AllocationResult::Ready(allocation) = request_allocation(
            &mut session,
            &private_target,
            Some(&invite),
            "00000000-0000-4000-8000-000000000001",
            42,
        )
        .unwrap() else {
            panic!("expected ready allocation")
        };
        assert_eq!(allocation.requested_vip_server_id, Some(77));
        assert_eq!(allocation.private_server_routing_id(), Some(issued_uuid));
        assert!(!format!("{allocation:?}").contains(issued_uuid));

        let mut numeric = ready_body(12);
        numeric["joinScript"]["PrivateServerID"] = json!(77);
        let (mut session, _) = mock_session(vec![wire(200, None, false, &numeric.to_string())]);
        assert!(request_allocation(
            &mut session,
            &private_target,
            Some(&invite),
            "00000000-0000-4000-8000-000000000001",
            42,
        )
        .is_err());

        let mut nil_uuid = ready_body(12);
        nil_uuid["joinScript"]["PrivateServerID"] = json!("00000000-0000-0000-0000-000000000000");
        let (mut session, _) = mock_session(vec![wire(200, None, false, &nil_uuid.to_string())]);
        assert!(request_allocation(
            &mut session,
            &private_target,
            Some(&invite),
            "00000000-0000-4000-8000-000000000001",
            42,
        )
        .is_err());
    }
    #[test]
    fn mismatched_place_private_server_and_public_job_are_rejected() {
        let (mut s, _) = mock_session(vec![wire(200, None, false, &ready_body(13).to_string())]);
        assert!(request_allocation(
            &mut s,
            &target(12),
            None,
            "00000000-0000-4000-8000-000000000001",
            42
        )
        .is_err());

        let invite = PrivateInvite::new(12, 88, 77, "invite-secret").unwrap();
        let private_target = JoinTarget {
            place_id: 12,
            is_playable: true,
            selection: Selection::UserSelectedPrivateServer,
            job_id: None,
        };
        let wrong_private = json!({"status":2,"joinScript":{"PlaceId":12,"PrivateServerID":78,"PrivateServerOwnerID":88}});
        let (mut s, _) = mock_session(vec![wire(200, None, false, &wrong_private.to_string())]);
        assert!(request_allocation(
            &mut s,
            &private_target,
            Some(&invite),
            "00000000-0000-4000-8000-000000000001",
            42
        )
        .is_err());

        let mut public_job = target(12);
        public_job.job_id = Some("00000000-0000-4000-8000-000000000003".into());
        let (mut s, _) = mock_session(vec![wire(200, None, false, &ready_body(12).to_string())]);
        assert!(request_allocation(
            &mut s,
            &public_job,
            None,
            "00000000-0000-4000-8000-000000000001",
            42
        )
        .is_err());

        let mut public_body = ready_body(12);
        public_body["joinScript"]["PrivateServerID"] = json!("");
        let (mut session, _) = mock_session(vec![wire(200, None, false, &public_body.to_string())]);
        let AllocationResult::Ready(public_allocation) = request_allocation(
            &mut session,
            &target(12),
            None,
            "00000000-0000-4000-8000-000000000001",
            42,
        )
        .unwrap() else {
            panic!("expected public allocation")
        };
        assert_eq!(public_allocation.private_server_routing_id(), None);
    }
    #[test]
    fn issued_account_identity_must_match_validated_account() {
        let body = ready_body(12);
        let (mut session, _) = mock_session(vec![wire(200, None, false, &body.to_string())]);
        assert!(request_allocation(
            &mut session,
            &target(12),
            None,
            "00000000-0000-4000-8000-000000000001",
            43,
        )
        .is_err());
    }

    #[test]
    fn public_server_ids_reject_nil_but_preserve_other_non_nil_uuid_shapes() {
        let nil = "00000000-0000-0000-0000-000000000000";
        let mut nil_target = target(12);
        nil_target.job_id = Some(nil.into());
        let (mut session, mock) = mock_session(vec![]);
        assert!(request_allocation(
            &mut session,
            &nil_target,
            None,
            "00000000-0000-4000-8000-000000000001",
            42,
        )
        .is_err());
        assert!(mock.requests.lock().unwrap().is_empty());

        let unusual_non_nil = "ffffffff-ffff-ffff-ffff-ffffffffffff";
        let mut public_target = target(12);
        public_target.job_id = Some(unusual_non_nil.into());
        let mut body = ready_body(12);
        body["joinScript"]["GameId"] = json!(unusual_non_nil);
        let (mut session, _) = mock_session(vec![wire(200, None, false, &body.to_string())]);
        assert!(matches!(
            request_allocation(
                &mut session,
                &public_target,
                None,
                "00000000-0000-4000-8000-000000000001",
                42,
            )
            .unwrap(),
            AllocationResult::Ready(_)
        ));

        body["joinScript"]["GameId"] = json!(nil);
        let (mut session, _) = mock_session(vec![wire(200, None, false, &body.to_string())]);
        assert!(request_allocation(
            &mut session,
            &public_target,
            None,
            "00000000-0000-4000-8000-000000000001",
            42,
        )
        .is_err());
    }
    #[test]
    fn malformed_transport_credentials_prevent_transport_readiness_and_debug_redacts() {
        for (bad_token, bad_pin) in [(true, false), (false, true)] {
            let mut body = ready_body(12);
            let join = body["joinScript"].as_object_mut().unwrap();
            if bad_token {
                join.insert("NetStackTokenValue".into(), json!("not-base64!"));
            }
            if bad_pin {
                join.insert(
                    "NetStackConfig".into(),
                    json!("%7B%22port%22%3A443%2C%22certHashes%22%3A[%22bad%22]%7D"),
                );
            }
            let (mut s, _) = mock_session(vec![wire(200, None, false, &body.to_string())]);
            let AllocationResult::Ready(a) = request_allocation(
                &mut s,
                &target(12),
                None,
                "00000000-0000-4000-8000-000000000001",
                42,
            )
            .unwrap() else {
                panic!("expected HTTP-ready allocation")
            };
            assert!(!a.is_transport_ready());
            let debug = format!("{a:?} {:?}", a.tickets);
            assert!(!debug.contains("ticket-secret"));
            assert!(!debug.contains("not-base64"));
        }
        let secret_body = json!({"ClientTicket":"do-not-print"});
        let (mut s, _) = mock_session(vec![wire(200, None, false, &secret_body.to_string())]);
        let reply = s
            .request("gamejoin.roblox.com", "/v1/x", "GET", None, false)
            .unwrap();
        assert!(!format!("{reply:?}").contains("do-not-print"));
    }
    #[test]
    fn csrf_and_pin_inputs_are_capped() {
        let giant_csrf = "c".repeat(MAX_CSRF_TOKEN + 1);
        let (mut s, mock) = mock_session(vec![wire(403, Some(&giant_csrf), false, "{}")]);
        let reply = s
            .request("gamejoin.roblox.com", "/v1/x", "GET", None, true)
            .unwrap();
        assert_eq!(reply.http_status, 403);
        assert_eq!(mock.requests.lock().unwrap().len(), 1);

        let pin = base64::engine::general_purpose::STANDARD.encode([3u8; 32]);
        let config = json!({"port":443,"certHashes":vec![pin; MAX_CERT_PINS + 1]});
        let encoded: String = config
            .to_string()
            .bytes()
            .map(|b| format!("%{b:02X}"))
            .collect();
        let join = serde_json::Map::from_iter([
            ("NetStackConfig".into(), json!(encoded)),
            ("NetStackPort".into(), json!(443)),
        ]);
        let (pins, port) = parse_net_stack(&join);
        assert!(pins.is_empty());
        assert_eq!(port, Some(443));
    }
    #[test]
    fn allocation_fields_only_come_from_returned_join() {
        let body = json!({"status":2,"joinTicket":"issued-ticket","joinScript":{"PlaceId":12,"UserId":42,"UserName":"tester","DisplayName":"Tester","SessionId":"session-secret","GameId":"job","ClientTicket":"client-secret","MachineAddress":"10.0.0.5","UdmuxEndpoints":[{"Address":"8.8.8.8"}],"NetStackPort":443,"NetStackTokenValue":base64::engine::general_purpose::STANDARD.encode([7u8;16]),"NetStackConfig":format!("%7B%22port%22%3A443%2C%22certHashes%22%3A[%22{}%22]%7D",base64::engine::general_purpose::STANDARD.encode([9u8;32]))}});
        let (mut s, _) = mock_session(vec![wire(200, None, false, &body.to_string())]);
        let target = JoinTarget {
            place_id: 12,
            is_playable: true,
            selection: Selection::UserSelectedPlace,
            job_id: None,
        };
        let result = request_allocation(
            &mut s,
            &target,
            None,
            "00000000-0000-4000-8000-000000000001",
            42,
        )
        .unwrap();
        let AllocationResult::Ready(a) = result else {
            panic!("not ready")
        };
        assert!(a.is_transport_ready());
        assert_eq!(a.token(), Some(&[7u8; 16]));
        assert_eq!(a.tickets.client_ticket(), Some("client-secret"));
        assert_eq!(a.user_id, 42);
        assert_eq!(a.session_id(), "session-secret");
        assert!(!format!("{a:?}").contains("client-secret"));
    }
}
