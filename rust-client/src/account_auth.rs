//! Ordinary account-session loading and identity confirmation.
//!
//! Constructing an account session only reads and validates a local private
//! cookie. `validate` sends one normal authenticated-user GET and accepts only
//! a non-challenged JSON identity response. It does not answer game-server
//! authentication messages or synthesize account state.

use crate::allocation::{HttpReply, RobloxSession, SessionError};
use serde_json::Value;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::Path;

const COOKIE_LIMIT: usize = 32 * 1024;
const IDENTITY_TEXT_LIMIT: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountError(&'static str);

impl AccountError {
    const fn new(message: &'static str) -> Self {
        Self(message)
    }
}

impl fmt::Display for AccountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for AccountError {}

impl From<SessionError> for AccountError {
    fn from(_: SessionError) -> Self {
        // Session errors may acquire upstream detail in the future. Keep this
        // boundary safe for CLI/UI reporting even if a transport regresses.
        Self::new("Could not create an account session")
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct AuthenticatedIdentity {
    user_id: u64,
    name: String,
    display_name: String,
}

impl AuthenticatedIdentity {
    pub fn user_id(&self) -> u64 {
        self.user_id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn display_name(&self) -> &str {
        &self.display_name
    }
}

impl fmt::Debug for AuthenticatedIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthenticatedIdentity")
            .field("user_id", &"[VERIFIED]")
            .field("name", &"[REDACTED]")
            .field("display_name", &"[REDACTED]")
            .finish()
    }
}

/// A locally loaded account session. Its identity is absent until Roblox
/// confirms it with the authenticated-user endpoint.
pub struct AccountSession {
    session: RobloxSession,
    identity: Option<AuthenticatedIdentity>,
}

impl fmt::Debug for AccountSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccountSession")
            .field("session", &"[REDACTED]")
            .field("identity", &self.identity)
            .finish()
    }
}

impl AccountSession {
    /// Load an explicit account credential file without performing any
    /// network request.
    pub fn from_private_file(path: impl AsRef<Path>) -> Result<Self, AccountError> {
        let cookie = read_private_cookie(path.as_ref())?;
        Self::from_cookie(&cookie)
    }

    /// Load `secrets/roblox-cookie.txt` beneath the supplied project root.
    pub fn from_project_root(root: impl AsRef<Path>) -> Result<Self, AccountError> {
        Self::from_private_file(root.as_ref().join("secrets/roblox-cookie.txt"))
    }

    /// Load the configured session root or the platform's local account files.
    pub fn from_default_project() -> Result<Self, AccountError> {
        if let Some(root) = std::env::var_os("RCR_SESSION_ROOT") {
            return Self::from_project_root(root);
        }
        #[cfg(windows)]
        {
            let local = std::env::var_os("LOCALAPPDATA").ok_or_else(|| {
                AccountError::new("Use --credential-file when LOCALAPPDATA is unavailable")
            })?;
            return Self::from_project_root(std::path::PathBuf::from(local).join("rove"));
        }
        #[cfg(not(windows))]
        {
            let root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .ok_or_else(|| AccountError::new("Project root is unavailable"))?;
            Self::from_project_root(root)
        }
    }

    /// Construct from a value obtained directly from the user or a secret
    /// store. The string is not retained by this module after RobloxSession
    /// has taken its redacted secret copy; no network request is performed.
    pub fn from_cookie(cookie: &str) -> Result<Self, AccountError> {
        Ok(Self {
            session: RobloxSession::new(cookie)?,
            identity: None,
        })
    }

    /// Confirm the account through Roblox's ordinary authenticated-user API.
    /// A login challenge, non-200 response, malformed body, or absent identity
    /// is reported as an error with no response content or credential details.
    pub fn validate(&mut self) -> Result<&AuthenticatedIdentity, AccountError> {
        if self.identity.is_none() {
            let reply = self
                .session
                .request(
                    "users.roblox.com",
                    "/v1/users/authenticated",
                    "GET",
                    None,
                    false,
                )
                .map_err(|_| AccountError::new("Could not verify the account session"))?;
            self.identity = Some(parse_authenticated_identity(&reply)?);
        }
        Ok(self
            .identity
            .as_ref()
            .expect("identity was set by validation"))
    }

    /// The id is available only after successful response validation.
    pub fn user_id(&self) -> Option<u64> {
        self.identity.as_ref().map(AuthenticatedIdentity::user_id)
    }

    pub fn identity(&self) -> Option<&AuthenticatedIdentity> {
        self.identity.as_ref()
    }

    pub fn session(&mut self) -> &mut RobloxSession {
        &mut self.session
    }

    pub fn into_parts(self) -> (RobloxSession, AuthenticatedIdentity) {
        let identity = self
            .identity
            .expect("account must be validated before joining");
        (self.session, identity)
    }
}

fn parse_authenticated_identity(reply: &HttpReply) -> Result<AuthenticatedIdentity, AccountError> {
    if reply.challenge_present {
        return Err(AccountError::new("Roblox requires an account challenge"));
    }
    if reply.http_status != 200 {
        return Err(AccountError::new(
            "Roblox did not authenticate this account session",
        ));
    }
    if !reply
        .content_type
        .split(';')
        .next()
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"))
    {
        return Err(AccountError::new(
            "Roblox returned an unexpected account response",
        ));
    }
    let body = reply
        .data
        .as_ref()
        .and_then(Value::as_object)
        .ok_or_else(|| AccountError::new("Roblox returned an unexpected account response"))?;
    let user_id = body
        .get("id")
        .and_then(Value::as_u64)
        .filter(|id| *id > 0)
        .ok_or_else(|| AccountError::new("Roblox returned an unexpected account identity"))?;
    let name = bounded_identity_text(body.get("name"))
        .ok_or_else(|| AccountError::new("Roblox returned an unexpected account identity"))?;
    let display_name = body
        .get("displayName")
        .and_then(|value| bounded_identity_text(Some(value)))
        .unwrap_or_else(|| name.clone());
    Ok(AuthenticatedIdentity {
        user_id,
        name,
        display_name,
    })
}

fn bounded_identity_text(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?;
    if text.trim().is_empty()
        || text.len() > IDENTITY_TEXT_LIMIT
        || text.chars().any(char::is_control)
    {
        return None;
    }
    Some(text.to_owned())
}

fn read_private_cookie(path: &Path) -> Result<String, AccountError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

        // Reject a symlinked secrets directory before opening the final file.
        let parent = path
            .parent()
            .ok_or_else(|| AccountError::new("Account credential file is unavailable"))?;
        let parent_meta = fs::symlink_metadata(parent)
            .map_err(|_| AccountError::new("Account credential directory is unavailable"))?;
        let euid = unsafe { libc::geteuid() };
        if !parent_meta.file_type().is_dir()
            || parent_meta.uid() != euid
            || parent_meta.mode() & 0o077 != 0
        {
            return Err(AccountError::new(
                "Account credential directory must be private and owner-controlled",
            ));
        }

        let mut options = OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        let mut file = options
            .open(path)
            .map_err(|_| AccountError::new("Account credential file is unavailable"))?;
        validate_private_file(&file)?;
        read_cookie_bytes(&mut file)
    }
    #[cfg(windows)]
    {
        let mut file = rove_client_core::windows_private::open_read(path, COOKIE_LIMIT as u64)
            .map_err(|_| AccountError::new("Credential file and directory must be private, regular Windows files under 32 KiB"))?;
        read_cookie_bytes(&mut file)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        Err(AccountError::new(
            "Secure account credential loading is unsupported on this platform",
        ))
    }
}

#[cfg(unix)]
fn validate_private_file(file: &File) -> Result<(), AccountError> {
    use std::os::unix::fs::MetadataExt;
    let meta = file
        .metadata()
        .map_err(|_| AccountError::new("Account credential file is unavailable"))?;
    let euid = unsafe { libc::geteuid() };
    if !meta.is_file()
        || meta.uid() != euid
        || meta.mode() & 0o077 != 0
        || meta.len() > COOKIE_LIMIT as u64
    {
        return Err(AccountError::new(
            "Account credential must be an owner-only regular file under 32 KiB",
        ));
    }
    Ok(())
}

fn read_cookie_bytes(file: &mut File) -> Result<String, AccountError> {
    let mut bytes = Vec::with_capacity(COOKIE_LIMIT.min(4096));
    file.take((COOKIE_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| AccountError::new("Account credential file could not be read"))?;
    if bytes.len() > COOKIE_LIMIT {
        return Err(AccountError::new("Account credential exceeds 32 KiB"));
    }
    let value = std::str::from_utf8(&bytes)
        .map_err(|_| AccountError::new("Account credential must be UTF-8"))?
        .trim();
    if value.is_empty() {
        return Err(AccountError::new("Account credential file is empty"));
    }
    Ok(value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;

    fn reply(status: u16, challenge: bool, content_type: &str, data: Option<Value>) -> HttpReply {
        HttpReply {
            http_status: status,
            challenge_present: challenge,
            content_type: content_type.to_owned(),
            data,
        }
    }

    #[test]
    fn accepts_only_a_nonchallenged_positive_account_identity() {
        let accepted = parse_authenticated_identity(&reply(
            200,
            false,
            "application/json; charset=utf-8",
            Some(json!({"id":123,"name":"AltName","displayName":"Alt Display"})),
        ))
        .unwrap();
        assert_eq!(accepted.user_id(), 123);
        assert_eq!(accepted.name(), "AltName");
        assert_eq!(accepted.display_name(), "Alt Display");

        let fallback = parse_authenticated_identity(&reply(
            200,
            false,
            "application/json",
            Some(json!({"id":123,"name":"AltName"})),
        ))
        .unwrap();
        assert_eq!(fallback.display_name(), "AltName");
    }

    #[test]
    fn rejects_http_errors_challenges_and_malformed_identity() {
        let cases = [
            reply(
                403,
                false,
                "application/json",
                Some(json!({"id":123,"name":"Alt"})),
            ),
            reply(
                200,
                true,
                "application/json",
                Some(json!({"id":123,"name":"Alt"})),
            ),
            reply(
                200,
                false,
                "text/html",
                Some(json!({"id":123,"name":"Alt"})),
            ),
            reply(
                200,
                false,
                "application/json",
                Some(json!({"id":0,"name":"Alt"})),
            ),
            reply(
                200,
                false,
                "application/json",
                Some(json!({"id":true,"name":"Alt"})),
            ),
            reply(
                200,
                false,
                "application/json",
                Some(json!({"id":123,"name":"\n"})),
            ),
            reply(200, false, "application/json", None),
        ];
        for item in cases {
            assert!(parse_authenticated_identity(&item).is_err());
        }
    }

    #[test]
    fn cookie_and_account_debug_never_expose_credentials_or_names() {
        let marker = "synthetic-cookie-secret-not-real";
        let account = AccountSession::from_cookie(marker).unwrap();
        assert_eq!(account.user_id(), None);
        assert!(!format!("{account:?}").contains(marker));
        assert!(!format!("{:?}", account.session).contains(marker));

        let identity = parse_authenticated_identity(&reply(
            200,
            false,
            "application/json",
            Some(json!({"id":123,"name":"SyntheticPrivateName"})),
        ))
        .unwrap();
        assert!(!format!("{identity:?}").contains("SyntheticPrivateName"));
    }

    #[cfg(unix)]
    #[test]
    fn reads_only_owner_private_regular_cookie_file_and_redacts_failures() {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let directory = std::env::temp_dir().join(format!(
            "rcr-account-auth-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let path = directory.join("roblox-cookie.txt");
        let mut options = OpenOptions::new();
        options.create_new(true).write(true).mode(0o600);
        let mut file = options.open(&path).unwrap();
        file.write_all(b"synthetic-cookie\n").unwrap();
        drop(file);
        assert_eq!(read_private_cookie(&path).unwrap(), "synthetic-cookie");

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_private_cookie(&path).is_err());
        let leaked_symlink = directory.join("link-cookie.txt");
        std::os::unix::fs::symlink(&path, &leaked_symlink).unwrap();
        assert!(read_private_cookie(&leaked_symlink).is_err());
        fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_loads_private_credentials_and_private_links_without_network() {
        let directory = std::env::temp_dir().join(format!(
            "rove-credential-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        drop(rove_client_core::windows_private::create_directory(&directory).unwrap());
        let cookie = directory.join("synthetic-cookie.txt");
        let mut file = rove_client_core::windows_private::create_file(&cookie, false).unwrap();
        file.write_all(b"synthetic-cookie\r\n").unwrap();
        drop(file);
        assert_eq!(read_private_cookie(&cookie).unwrap(), "synthetic-cookie");
        let link = directory.join("synthetic-link.json");
        let url = "https://www.roblox.com/share?code=0123456789abcdef0123456789abcdef&type=Server";
        let mut file = rove_client_core::windows_private::create_file(&link, false).unwrap();
        serde_json::to_writer(&mut file, &serde_json::json!({"url":url})).unwrap();
        drop(file);
        assert_eq!(
            crate::join_coordinator::read_private_share_link(&link).unwrap(),
            url
        );
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&cookie)
            .unwrap();
        file.write_all(&vec![b'x'; COOKIE_LIMIT]).unwrap();
        drop(file);
        let error = read_private_cookie(&cookie).unwrap_err().to_string();
        assert!(!error.contains("synthetic-cookie"));
        std::fs::remove_dir_all(directory).unwrap();
    }
}
