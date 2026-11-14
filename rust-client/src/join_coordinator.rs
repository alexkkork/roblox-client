//! Bounded orchestration for the ordinary account join-allocation endpoint.
//!
//! This owns the HTTPS pending-response policy only. It does not connect to a
//! game server, process protocol authentication, or alter server-issued values.

use crate::allocation::{
    new_join_attempt_id, request_allocation, AllocationResult, IssuedJoinAllocation, JoinTarget,
    PrivateInvite, RobloxSession, Selection, SessionError,
};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use std::{fs::OpenOptions, io::Read, path::Path};

const MAX_SAFE_ID: u64 = 9_007_199_254_740_991;

/// Resolve an explicit place or choose the first public place from the
/// authenticated account's own games list, then verify playability over the
/// ordinary games API. It never falls back to an unrelated public place.
pub fn lookup_target(
    session: &mut RobloxSession,
    explicit_place_id: Option<u64>,
    job_id: Option<&str>,
    authenticated_user_id: u64,
) -> Result<JoinTarget, SessionError> {
    if authenticated_user_id == 0 || authenticated_user_id > MAX_SAFE_ID {
        return Err(SessionError::new("Authenticated account id is invalid"));
    }
    if job_id.is_some() && explicit_place_id.is_none() {
        return Err(SessionError::new("A server id requires an explicit place"));
    }
    let (place_id, selection) = if let Some(id) = explicit_place_id {
        if id == 0 || id > MAX_SAFE_ID {
            return Err(SessionError::new("Place id is invalid"));
        }
        (id, Selection::UserSelectedPlace)
    } else {
        let reply = session.request(
            "games.roblox.com",
            &format!(
            "/v2/users/{authenticated_user_id}/games?accessFilter=Public&sortOrder=Asc&limit=10"),
            "GET",
            None,
            false,
        )?;
        let rows = checked_data(&reply, "Could not list account-owned public places")?
            .get("data")
            .and_then(Value::as_array)
            .ok_or(SessionError::new(
                "Public place response had an unsupported structure",
            ))?;
        let place_id = rows
            .iter()
            .find_map(|game| game.get("rootPlace")?.get("id")?.as_u64())
            .filter(|id| *id > 0 && *id <= MAX_SAFE_ID)
            .ok_or(SessionError::new("No account-owned public place was found"))?;
        (place_id, Selection::AltOwnedPublicPlace)
    };
    if let Some(job) = job_id {
        if !canonical_uuid(job) {
            return Err(SessionError::new("Public server id is invalid"));
        }
    }
    verify_playable(session, place_id)?;
    Ok(JoinTarget {
        place_id,
        is_playable: true,
        selection,
        job_id: job_id.map(str::to_owned),
    })
}

/// Resolve an explicitly supplied Roblox server share URL. The code is kept
/// only in the redacted PrivateInvite and is never returned in progress data.
pub fn resolve_private_share_link(
    session: &mut RobloxSession,
    share_url: &str,
) -> Result<(JoinTarget, PrivateInvite), SessionError> {
    let link_code = parse_share_code(share_url)?;
    let reply = session.request(
        "apis.roblox.com",
        "/sharelinks/v1/resolve-link",
        "POST",
        Some(&json!({"linkId":link_code,"linkType":"Server"})),
        true,
    )?;
    let data = checked_data(&reply, "Roblox did not resolve the private server invite")?;
    let invite_data = data
        .get("privateServerInviteData")
        .and_then(Value::as_object)
        .filter(|v| v.get("status").and_then(Value::as_str) == Some("Valid"))
        .ok_or(SessionError::new(
            "Private server invite is not valid for this account",
        ))?;
    let place_id = invite_data
        .get("placeId")
        .and_then(Value::as_u64)
        .filter(|id| *id > 0 && *id <= MAX_SAFE_ID)
        .ok_or(SessionError::new(
            "Private invite has an unsupported structure",
        ))?;
    let owner_user_id = invite_data
        .get("ownerUserId")
        .and_then(Value::as_u64)
        .filter(|id| *id > 0)
        .ok_or(SessionError::new(
            "Private invite has an unsupported structure",
        ))?;
    let private_server_id = invite_data
        .get("privateServerId")
        .and_then(Value::as_u64)
        .filter(|id| *id > 0)
        .ok_or(SessionError::new(
            "Private invite has an unsupported structure",
        ))?;
    let link_code =
        invite_data
            .get("linkCode")
            .and_then(Value::as_str)
            .ok_or(SessionError::new(
                "Private invite has an unsupported structure",
            ))?;
    let invite = PrivateInvite::new(place_id, owner_user_id, private_server_id, link_code)?;
    verify_playable(session, place_id)?;
    Ok((
        JoinTarget {
            place_id,
            is_playable: true,
            selection: Selection::UserSelectedPrivateServer,
            job_id: None,
        },
        invite,
    ))
}

/// Resolve a private server owned by the authenticated account. The local
/// file supplies only the claimed place/server ids; the access code is fetched
/// fresh from Roblox's owner-scoped server list and kept redacted.
pub fn resolve_owned_private_server_file(
    session: &mut RobloxSession,
    path: impl AsRef<Path>,
    authenticated_user_id: u64,
) -> Result<(JoinTarget, PrivateInvite), SessionError> {
    if authenticated_user_id == 0 || authenticated_user_id > MAX_SAFE_ID {
        return Err(SessionError::new("Authenticated account id is invalid"));
    }
    let metadata = read_private_json(path)?;
    let (place_id, universe_id, vip_server_id) =
        owned_private_server_ids(&metadata, authenticated_user_id)?;
    let access_code =
        find_owned_server_access_code(session, place_id, vip_server_id, authenticated_user_id)?;
    verify_owned_server_details(session, place_id, universe_id, vip_server_id)?;
    let invite = PrivateInvite::new_owned_access_code(
        place_id,
        authenticated_user_id,
        vip_server_id,
        &access_code,
    )?;
    Ok((
        JoinTarget {
            place_id,
            is_playable: true,
            selection: Selection::UserSelectedPrivateServer,
            job_id: None,
        },
        invite,
    ))
}

fn owned_private_server_ids(
    metadata: &Value,
    authenticated_user_id: u64,
) -> Result<(u64, u64, u64), SessionError> {
    let object = metadata.as_object().ok_or(SessionError::new(
        "Owned private server metadata has an unsupported structure",
    ))?;
    let claimed_user_id = object
        .get("owner_user_id")
        .and_then(Value::as_u64)
        .filter(|id| *id > 0 && *id <= MAX_SAFE_ID)
        .ok_or(SessionError::new(
            "Owned private server metadata has an unsupported structure",
        ))?;
    if claimed_user_id != authenticated_user_id {
        return Err(SessionError::new(
            "Owned private server metadata belongs to a different account",
        ));
    }
    let place_id = object
        .get("place_id")
        .and_then(Value::as_u64)
        .filter(|id| *id > 0 && *id <= MAX_SAFE_ID)
        .ok_or(SessionError::new(
            "Owned private server metadata has an unsupported structure",
        ))?;
    let universe_id = object
        .get("universe_id")
        .and_then(Value::as_u64)
        .filter(|id| *id > 0 && *id <= MAX_SAFE_ID)
        .ok_or(SessionError::new(
            "Owned private server metadata has an unsupported structure",
        ))?;
    let created = object
        .get("created_server")
        .and_then(Value::as_object)
        .ok_or(SessionError::new(
            "Owned private server metadata has an unsupported structure",
        ))?;
    let created_owner = created
        .get("owner")
        .and_then(Value::as_object)
        .and_then(|owner| owner.get("id"))
        .and_then(Value::as_u64);
    if created_owner != Some(authenticated_user_id) {
        return Err(SessionError::new(
            "Owned private server metadata belongs to a different account",
        ));
    }
    let vip_server_id = created
        .get("vipServerId")
        .and_then(Value::as_u64)
        .filter(|id| *id > 0)
        .ok_or(SessionError::new(
            "Owned private server metadata has an unsupported structure",
        ))?;
    Ok((place_id, universe_id, vip_server_id))
}

fn metadata_u64(value: &Value) -> Option<u64> {
    value.as_u64()
}

fn find_owned_server_access_code(
    session: &mut RobloxSession,
    place_id: u64,
    vip_server_id: u64,
    authenticated_user_id: u64,
) -> Result<String, SessionError> {
    find_owned_server_access_code_with(vip_server_id, authenticated_user_id, |cursor| {
        let mut path = format!("/v1/games/{place_id}/private-servers?limit=100&sortOrder=Asc");
        if let Some(value) = cursor {
            let encoded: String = url::form_urlencoded::byte_serialize(value.as_bytes()).collect();
            path.push_str("&cursor=");
            path.push_str(&encoded);
        }
        let reply = session.request("games.roblox.com", &path, "GET", None, false)?;
        checked_data(&reply, "Could not list owned private servers").cloned()
    })
}

fn find_owned_server_access_code_with(
    vip_server_id: u64,
    authenticated_user_id: u64,
    mut fetch_page: impl FnMut(Option<&str>) -> Result<Value, SessionError>,
) -> Result<String, SessionError> {
    const MAX_PAGES: usize = 10;
    const MAX_CURSOR: usize = 4096;
    let mut cursor: Option<String> = None;
    let mut seen_cursors = std::collections::HashSet::new();
    for _ in 0..MAX_PAGES {
        if let Some(value) = cursor.as_deref() {
            if value.len() > MAX_CURSOR || !seen_cursors.insert(value.to_owned()) {
                return Err(SessionError::new(
                    "Private server listing pagination was invalid",
                ));
            }
        }
        let page = fetch_page(cursor.as_deref())?;
        let data = page.as_object().ok_or(SessionError::new(
            "Private server listing had an unsupported structure",
        ))?;
        let rows = data
            .get("data")
            .and_then(Value::as_array)
            .ok_or(SessionError::new(
                "Private server listing had an unsupported structure",
            ))?;
        for row in rows {
            if row.get("vipServerId").and_then(Value::as_u64) != Some(vip_server_id) {
                continue;
            }
            let owner_id = row
                .get("owner")
                .and_then(Value::as_object)
                .and_then(|owner| owner.get("id"))
                .and_then(Value::as_u64);
            if owner_id != Some(authenticated_user_id) {
                return Err(SessionError::new(
                    "Private server listing did not confirm account ownership",
                ));
            }
            return row_access_code(row);
        }
        match data.get("nextPageCursor") {
            None | Some(Value::Null) => break,
            Some(Value::String(next)) if next.is_empty() => break,
            Some(Value::String(next)) if next.len() <= MAX_CURSOR => cursor = Some(next.clone()),
            _ => {
                return Err(SessionError::new(
                    "Private server listing pagination was invalid",
                ))
            }
        }
    }
    Err(SessionError::new(
        "The selected private server was not found in the account-owned list",
    ))
}

fn row_access_code(row: &Value) -> Result<String, SessionError> {
    row.get("accessCode")
        .and_then(Value::as_str)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 4096
                && !value.bytes().any(|byte| byte <= 32 || byte == 127)
        })
        .map(str::to_owned)
        .ok_or(SessionError::new(
            "Roblox did not issue an access code for this private server",
        ))
}

fn verify_owned_server_details(
    session: &mut RobloxSession,
    place_id: u64,
    universe_id: u64,
    vip_server_id: u64,
) -> Result<(), SessionError> {
    let reply = session.request(
        "games.roblox.com",
        &format!("/v1/vip-servers/{vip_server_id}"),
        "GET",
        None,
        false,
    )?;
    let data = checked_data(&reply, "Could not verify owned private server")?;
    if !owned_server_details_match(data, place_id, universe_id, vip_server_id) {
        return Err(SessionError::new(
            "Owned private server details did not match the selected active place",
        ));
    }
    Ok(())
}

fn owned_server_details_match(
    data: &Value,
    place_id: u64,
    universe_id: u64,
    vip_server_id: u64,
) -> bool {
    data.get("id").and_then(Value::as_u64) == Some(vip_server_id)
        && data.get("active").and_then(Value::as_bool) == Some(true)
        && data
            .get("game")
            .and_then(Value::as_object)
            .and_then(|game| game.get("rootPlace"))
            .and_then(Value::as_object)
            .and_then(|root| root.get("id"))
            .and_then(Value::as_u64)
            == Some(place_id)
        && data
            .get("game")
            .and_then(Value::as_object)
            .and_then(|game| game.get("id"))
            .and_then(Value::as_u64)
            == Some(universe_id)
}

fn parse_share_code(share_url: &str) -> Result<String, SessionError> {
    let url = url::Url::parse(share_url)
        .map_err(|_| SessionError::new("Expected a Roblox server share link"))?;
    if url.scheme() != "https"
        || url.host_str() != Some("www.roblox.com")
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || !matches!(url.path(), "/share" | "/share-links")
    {
        return Err(SessionError::new("Expected a Roblox server share link"));
    }
    let pairs: Vec<_> = url.query_pairs().collect();
    let types: Vec<_> = pairs.iter().filter(|(key, _)| key == "type").collect();
    let codes: Vec<_> = pairs.iter().filter(|(key, _)| key == "code").collect();
    if pairs.len() != 2
        || types.len() != 1
        || types[0].1 != "Server"
        || codes.len() != 1
        || codes[0].1.len() != 32
        || !codes[0].1.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(SessionError::new("Expected a Roblox server share link"));
    }
    Ok(codes[0].1.to_string())
}

/// Non-sensitive shape check used by local CLI validation.
pub(crate) fn valid_share_link_shape(value: &str) -> bool {
    parse_share_code(value).is_ok()
}

/// One bounded, no-follow, owner-only reader shared by CLI and file-based
/// coordinator entrypoints. The URL itself is never included in errors.
pub fn read_private_share_link(path: impl AsRef<Path>) -> Result<String, SessionError> {
    let value = read_private_json(path)?;
    let link = value
        .as_object()
        .filter(|object| object.len() == 1)
        .and_then(|object| object.get("url"))
        .and_then(Value::as_str)
        .filter(|text| text.len() <= 2048)
        .ok_or(SessionError::new(
            "Private target file had an unsupported structure",
        ))?;
    parse_share_code(link)?;
    Ok(link.to_owned())
}

fn read_private_json(path: impl AsRef<Path>) -> Result<Value, SessionError> {
    const LIMIT: usize = 16 * 1024;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    #[cfg(windows)]
    {
        let file = rove_client_core::windows_private::open_read(path.as_ref(), LIMIT as u64)
            .map_err(|_| {
                SessionError::new(
                    "Private target file must have a private Windows ACL and be under 16 KiB",
                )
            })?;
        let mut bytes = Vec::new();
        file.take((LIMIT + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| SessionError::new("Private target file could not be read"))?;
        if bytes.len() > LIMIT {
            return Err(SessionError::new(
                "Private target file exceeded the size limit",
            ));
        }
        serde_json::from_slice(&bytes)
            .map_err(|_| SessionError::new("Private target file had an unsupported structure"))
    }
    #[cfg(not(any(unix, windows)))]
    return Err(SessionError::new(
        "Secure private target loading is unsupported on this platform",
    ));

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mut file = options
            .open(path)
            .map_err(|_| SessionError::new("Private target file is unavailable"))?;
        let metadata = file
            .metadata()
            .map_err(|_| SessionError::new("Private target file is unavailable"))?;
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || metadata.len() > LIMIT as u64
        {
            return Err(SessionError::new(
                "Private target file must be owner-only and under 16 KiB",
            ));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take((LIMIT + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| SessionError::new("Private target file could not be read"))?;
        if bytes.len() > LIMIT {
            return Err(SessionError::new(
                "Private target file exceeded the size limit",
            ));
        }
        serde_json::from_slice(&bytes)
            .map_err(|_| SessionError::new("Private target file had an unsupported structure"))
    }
}

fn canonical_uuid(value: &str) -> bool {
    let mut nonzero = false;
    value.len() == 36
        && value.bytes().enumerate().all(|(i, b)| {
            if [8, 13, 18, 23].contains(&i) {
                b == b'-'
            } else {
                let valid = b.is_ascii_hexdigit();
                nonzero |= valid && b != b'0';
                valid
            }
        })
        && nonzero
}

/// Read the existing private share-link JSON file format without placing the
/// secret invite URL in CLI arguments or progress output.
pub fn resolve_private_share_file(
    session: &mut RobloxSession,
    path: impl AsRef<Path>,
) -> Result<(JoinTarget, PrivateInvite), SessionError> {
    let link = read_private_share_link(path)?;
    resolve_private_share_link(session, &link)
}

fn checked_data<'a>(
    reply: &'a crate::allocation::HttpReply,
    error: &'static str,
) -> Result<&'a Value, SessionError> {
    if reply.challenge_present {
        return Err(SessionError::new("Roblox requires an account challenge"));
    }
    if reply.http_status != 200 {
        return Err(SessionError::new(error));
    }
    reply.data.as_ref().ok_or(SessionError::new(error))
}

fn verify_playable(session: &mut RobloxSession, place_id: u64) -> Result<(), SessionError> {
    let reply = session.request(
        "games.roblox.com",
        &format!("/v1/games/multiget-place-details?placeIds={place_id}"),
        "GET",
        None,
        false,
    )?;
    let rows = checked_data(&reply, "Could not verify place playability")?
        .as_array()
        .ok_or(SessionError::new(
            "Place details response had an unsupported structure",
        ))?;
    if !is_verified_playable_row(rows, place_id) {
        return Err(SessionError::new("Place is not verified playable"));
    }
    Ok(())
}

fn is_verified_playable_row(rows: &[Value], place_id: u64) -> bool {
    rows.len() == 1
        && rows[0].get("placeId").and_then(Value::as_u64) == Some(place_id)
        && rows[0].get("isPlayable").and_then(Value::as_bool) == Some(true)
}

pub const MAX_ALLOCATION_ATTEMPTS: u8 = 8;
pub const PENDING_RETRY_DELAY: Duration = Duration::from_secs(3);
const CANCEL_POLL: Duration = Duration::from_millis(50);

/// Cloneable cancellation shared between the session owner and UI commands.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProgressState {
    Pending,
    Challenge,
    Rejected,
    Ready,
}

/// Sanitized progress contains status only, never the request, response body,
/// endpoint, ticket, private invite code, or transport token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JoinProgress {
    pub attempt: u8,
    pub http_status: u16,
    pub status: Option<i64>,
    pub state: ProgressState,
}

#[derive(Debug)]
pub enum JoinOutcome {
    Cancelled {
        attempts: u8,
    },
    PendingExhausted {
        attempts: u8,
        http_status: u16,
        status: i64,
    },
    Challenge {
        attempts: u8,
        http_status: u16,
    },
    Rejected {
        attempts: u8,
        http_status: u16,
        status: Option<i64>,
    },
    Ready {
        attempts: u8,
        allocation: IssuedJoinAllocation,
    },
}

/// Repeated allocation is allowed only for an explicit pending response.
/// The attempt identifier is generated once and remains fixed for this attempt.
/// Cancelling does not interrupt an in-progress HTTPS request; the existing
/// transport enforces its own bounded request timeout.
pub fn request_until_ready(
    session: &mut RobloxSession,
    target: &JoinTarget,
    invite: Option<&PrivateInvite>,
    authenticated_user_id: u64,
    cancellation: &CancellationToken,
    mut progress: impl FnMut(JoinProgress),
) -> Result<JoinOutcome, SessionError> {
    if cancellation.is_cancelled() {
        return Ok(JoinOutcome::Cancelled { attempts: 0 });
    }
    validate_target(target, invite)?;
    if authenticated_user_id == 0 || authenticated_user_id > MAX_SAFE_ID {
        return Err(SessionError::new("Authenticated account id is invalid"));
    }
    // Reconfirm through Roblox; a caller-created is_playable flag alone is
    // never accepted as proof at the allocation boundary.
    verify_playable(session, target.place_id)?;
    let attempt_id = new_join_attempt_id()?;
    run_policy(
        cancellation,
        |attempt| {
            let result =
                request_allocation(session, target, invite, &attempt_id, authenticated_user_id)?;
            progress(progress_for(attempt, &result));
            Ok(result)
        },
        sleep_cancellable,
    )
}

fn validate_target(
    target: &JoinTarget,
    invite: Option<&PrivateInvite>,
) -> Result<(), SessionError> {
    if !target.is_playable || target.place_id == 0 || target.place_id > 9_007_199_254_740_991 {
        return Err(SessionError::new("Target is not verified playable"));
    }
    if target
        .job_id
        .as_deref()
        .is_some_and(|job| !canonical_uuid(job))
    {
        return Err(SessionError::new("Public server id is invalid"));
    }
    match (target.selection, invite) {
        (Selection::UserSelectedPrivateServer, Some(invite))
            if invite.place_id == target.place_id
                && invite.private_server_id > 0
                && invite.owner_user_id > 0 => {}
        (Selection::UserSelectedPrivateServer, _) => {
            return Err(SessionError::new(
                "Private invite does not match selected place",
            ));
        }
        (_, Some(_)) => return Err(SessionError::new("Unexpected private invite")),
        _ => {}
    }
    Ok(())
}

fn progress_for(attempt: u8, result: &AllocationResult) -> JoinProgress {
    let (http_status, status, state) = match result {
        AllocationResult::Pending {
            http_status,
            status,
        } => (*http_status, Some(*status), ProgressState::Pending),
        AllocationResult::Challenge { http_status } => {
            (*http_status, None, ProgressState::Challenge)
        }
        AllocationResult::Rejected {
            http_status,
            status,
        } => (*http_status, *status, ProgressState::Rejected),
        AllocationResult::Ready(_) => (200, Some(2), ProgressState::Ready),
    };
    JoinProgress {
        attempt,
        http_status,
        status,
        state,
    }
}

fn run_policy(
    cancellation: &CancellationToken,
    mut request: impl FnMut(u8) -> Result<AllocationResult, SessionError>,
    mut wait: impl FnMut(Duration, &CancellationToken) -> bool,
) -> Result<JoinOutcome, SessionError> {
    let mut attempts = 0;
    while attempts < MAX_ALLOCATION_ATTEMPTS {
        if cancellation.is_cancelled() {
            return Ok(JoinOutcome::Cancelled { attempts });
        }
        attempts += 1;
        let result = request(attempts)?;
        if cancellation.is_cancelled() {
            return Ok(JoinOutcome::Cancelled { attempts });
        }
        match result {
            AllocationResult::Ready(allocation) => {
                return Ok(JoinOutcome::Ready {
                    attempts,
                    allocation,
                });
            }
            AllocationResult::Challenge { http_status } => {
                return Ok(JoinOutcome::Challenge {
                    attempts,
                    http_status,
                });
            }
            AllocationResult::Rejected {
                http_status,
                status,
            } => {
                return Ok(JoinOutcome::Rejected {
                    attempts,
                    http_status,
                    status,
                });
            }
            AllocationResult::Pending {
                http_status,
                status,
            } => {
                if attempts == MAX_ALLOCATION_ATTEMPTS {
                    return Ok(JoinOutcome::PendingExhausted {
                        attempts,
                        http_status,
                        status,
                    });
                }
                if !wait(PENDING_RETRY_DELAY, cancellation) {
                    return Ok(JoinOutcome::Cancelled { attempts });
                }
            }
        }
    }
    unreachable!("the allocation loop returns on its final attempt")
}

fn sleep_cancellable(delay: Duration, cancellation: &CancellationToken) -> bool {
    let deadline = Instant::now() + delay;
    while !cancellation.is_cancelled() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return true;
        }
        std::thread::sleep(remaining.min(CANCEL_POLL));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn share_link_parser_accepts_only_server_links_and_never_echoes_code() {
        let valid =
            "https://www.roblox.com/share?code=0123456789abcdef0123456789ABCDEF&type=Server";
        assert_eq!(
            parse_share_code(valid).unwrap(),
            "0123456789abcdef0123456789ABCDEF"
        );
        for invalid in [
            "http://www.roblox.com/share?code=0123456789abcdef0123456789abcdef&type=Server",
            "https://evil.example/share?code=0123456789abcdef0123456789abcdef&type=Server",
            "https://www.roblox.com/share?code=0123456789abcdef0123456789abcdef&type=Server&code=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "https://www.roblox.com/share?code=0123456789abcdef0123456789abcdef&type=Experience",
        ] {
            let error = parse_share_code(invalid).unwrap_err();
            assert_eq!(error.to_string(), "Expected a Roblox server share link");
            assert!(!format!("{error:?}").contains("0123456789abcdef"));
        }
    }

    #[test]
    fn playability_requires_the_exact_returned_place_id() {
        assert!(is_verified_playable_row(
            &[json!({"placeId":12,"isPlayable":true})],
            12
        ));
        assert!(!is_verified_playable_row(&[json!({"isPlayable":true})], 12));
        assert!(!is_verified_playable_row(
            &[json!({"placeId":"12","isPlayable":true})],
            12
        ));
        assert!(!is_verified_playable_row(
            &[json!({"placeId":13,"isPlayable":true})],
            12
        ));
        assert!(!is_verified_playable_row(
            &[json!({"placeId":12,"isPlayable":false})],
            12
        ));
    }

    #[test]
    fn public_server_ids_reject_only_nil_uuid_among_canonical_forms() {
        assert!(!canonical_uuid("00000000-0000-0000-0000-000000000000"));
        // Public JobIds are shape-validated here; preserve nonnil UUIDs even
        // when their version or variant nibble is outside RFC 4122 ranges.
        assert!(canonical_uuid("ffffffff-ffff-ffff-ffff-ffffffffffff"));
    }

    fn owned_metadata(owner_id: u64) -> Value {
        json!({
            "owner_user_id": owner_id,
            "place_id": 6252985844_u64,
            "universe_id": 2298998220_u64,
            "created_server": {
                "vipServerId": 77,
                "owner": {"id": owner_id},
                "accessCode": "do-not-use-local-cached-code"
            },
            "server_details": {}
        })
    }

    #[test]
    fn owned_metadata_requires_strict_numeric_claimed_owner_and_server_ids() {
        assert_eq!(
            owned_private_server_ids(&owned_metadata(42), 42).unwrap(),
            (6252985844, 2298998220, 77)
        );
        assert!(owned_private_server_ids(&owned_metadata(41), 42).is_err());
        let mut string_id = owned_metadata(42);
        string_id["owner_user_id"] = json!("42");
        assert!(owned_private_server_ids(&string_id, 42).is_err());
        let mut string_vip = owned_metadata(42);
        string_vip["created_server"]["vipServerId"] = json!("77");
        assert!(owned_private_server_ids(&string_vip, 42).is_err());
    }

    #[test]
    fn owner_list_requires_exact_owner_and_fresh_access_code() {
        let wrong_owner = json!({"data":[{"vipServerId":77,"owner":{"id":41},"accessCode":"hidden"}],"nextPageCursor":null});
        let error =
            find_owned_server_access_code_with(77, 42, |_| Ok(wrong_owner.clone())).unwrap_err();
        assert!(error.to_string().contains("ownership"));
        assert!(!format!("{error:?}").contains("hidden"));

        let missing_code =
            json!({"data":[{"vipServerId":77,"owner":{"id":42}}],"nextPageCursor":null});
        let error =
            find_owned_server_access_code_with(77, 42, |_| Ok(missing_code.clone())).unwrap_err();
        assert!(error.to_string().contains("access code"));

        let mut calls = 0;
        let error = find_owned_server_access_code_with(77, 42, |cursor| {
            calls += 1;
            let next = format!("cursor-{calls}");
            let expected = (calls > 1).then(|| format!("cursor-{}", calls - 1));
            assert_eq!(cursor, expected.as_deref());
            Ok(json!({"data":[],"nextPageCursor":next}))
        })
        .unwrap_err();
        assert!(error.to_string().contains("not found"));
        assert_eq!(calls, 10);
    }

    #[test]
    fn owned_details_require_active_vip_and_matching_place_and_universe() {
        let details = json!({"id":77,"active":true,"game":{"id":2298998220u64,"rootPlace":{"id":6252985844u64}}});
        assert!(owned_server_details_match(
            &details, 6252985844, 2298998220, 77
        ));
        let mut inactive = details.clone();
        inactive["active"] = json!(false);
        assert!(!owned_server_details_match(
            &inactive, 6252985844, 2298998220, 77
        ));
        assert!(!owned_server_details_match(
            &details, 6252985845, 2298998220, 77
        ));
        assert!(!owned_server_details_match(
            &details, 6252985844, 2298998221, 77
        ));
    }

    #[cfg(unix)]
    #[test]
    fn private_target_file_uses_the_bounded_owner_only_no_follow_reader() {
        use std::{
            fs,
            os::unix::fs::{symlink, PermissionsExt},
            time::{SystemTime, UNIX_EPOCH},
        };
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base =
            std::env::temp_dir().join(format!("rcr-private-target-{}-{nonce}", std::process::id()));
        let target = base.with_extension("json");
        let link = base.with_extension("link");
        fs::write(&target, r#"{"url":"https://www.roblox.com/share?code=0123456789abcdef0123456789abcdef&type=Server"}"#).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_private_share_link(&target).is_ok());
        symlink(&target, &link).unwrap();
        assert!(read_private_share_link(&link).is_err());
        fs::remove_file(link).unwrap();
        fs::write(&target, vec![b'x'; 16 * 1024 + 1]).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_private_share_link(&target).is_err());
        fs::remove_file(target).unwrap();
    }

    fn pending() -> AllocationResult {
        AllocationResult::Pending {
            status: 1,
            http_status: 200,
        }
    }

    #[test]
    fn only_pending_retries_and_rejection_stops_immediately() {
        let cancellation = CancellationToken::new();
        let calls = Cell::new(0_u8);
        let result = run_policy(
            &cancellation,
            |_| {
                let next = calls.get() + 1;
                calls.set(next);
                Ok(if next < 3 {
                    pending()
                } else {
                    AllocationResult::Rejected {
                        http_status: 403,
                        status: Some(9),
                    }
                })
            },
            |delay, _| delay == PENDING_RETRY_DELAY,
        )
        .unwrap();
        assert!(matches!(
            result,
            JoinOutcome::Rejected {
                attempts: 3,
                http_status: 403,
                status: Some(9)
            }
        ));
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn challenge_is_never_retried() {
        let cancellation = CancellationToken::new();
        let calls = Cell::new(0_u8);
        let result = run_policy(
            &cancellation,
            |_| {
                calls.set(calls.get() + 1);
                Ok(AllocationResult::Challenge { http_status: 403 })
            },
            |_, _| panic!("challenge must not be retried"),
        )
        .unwrap();
        assert!(matches!(
            result,
            JoinOutcome::Challenge {
                attempts: 1,
                http_status: 403
            }
        ));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn pending_stops_after_eight_posts() {
        let cancellation = CancellationToken::new();
        let calls = Cell::new(0_u8);
        let result = run_policy(
            &cancellation,
            |_| {
                calls.set(calls.get() + 1);
                Ok(pending())
            },
            |_, _| true,
        )
        .unwrap();
        assert!(matches!(
            result,
            JoinOutcome::PendingExhausted {
                attempts: 8,
                http_status: 200,
                status: 1
            }
        ));
        assert_eq!(calls.get(), 8);
    }

    #[test]
    fn cancellation_before_post_or_during_pending_wait_stops() {
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let calls = Cell::new(0_u8);
        let result = run_policy(
            &cancellation,
            |_| {
                calls.set(calls.get() + 1);
                Ok(pending())
            },
            |_, _| true,
        )
        .unwrap();
        assert!(matches!(result, JoinOutcome::Cancelled { attempts: 0 }));
        assert_eq!(calls.get(), 0);

        let cancellation = CancellationToken::new();
        let result = run_policy(
            &cancellation,
            |_| Ok(pending()),
            |_, token| {
                token.cancel();
                false
            },
        )
        .unwrap();
        assert!(matches!(result, JoinOutcome::Cancelled { attempts: 1 }));
    }

    #[test]
    fn progress_projection_contains_no_secret_fields() {
        let ready = AllocationResult::Ready(crate::allocation::test_issued_allocation());
        let report = format!("{:?}", progress_for(1, &ready));
        assert!(report.contains("attempt: 1"));
        assert!(report.contains("Ready"));
        for secret in ["cookie", "ticket", "token", "endpoint", "private_server"] {
            assert!(!report.to_lowercase().contains(secret));
        }
    }

    #[test]
    fn target_validation_binds_private_invite_and_selection() {
        let private = JoinTarget {
            place_id: 123,
            is_playable: true,
            selection: Selection::UserSelectedPrivateServer,
            job_id: None,
        };
        let invite = PrivateInvite::new(123, 45, 67, "invite-code-secret").unwrap();
        assert!(validate_target(&private, Some(&invite)).is_ok());
        assert!(validate_target(&private, None).is_err());
        assert!(validate_target(
            &private,
            Some(&PrivateInvite::new(999, 45, 67, "invite-code-secret").unwrap())
        )
        .is_err());
        let public = JoinTarget {
            selection: Selection::UserSelectedPlace,
            ..private
        };
        assert!(validate_target(&public, Some(&invite)).is_err());
    }
}
