//! Bounded animation loading. Credentials stay in the account HTTP client.
use crate::allocation::RobloxSession;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::Read,
    path::{Component, Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

pub const MAX_CLIP_BYTES: usize = 8 * 1024 * 1024;
const MAX_BYTES: usize = animation_codec::MAX_INPUT;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AssetError {
    InvalidReference,
    LocalUnavailable,
    InvalidPath,
    TooLarge,
    AuthenticationRequired,
    Http(u16),
    Challenge,
    DeliveryFailed,
    InvalidDelivery,
    InvalidRedirect,
    DownloadFailed,
    DecodeFailed(&'static str),
}
impl AssetError {
    pub fn code(&self) -> String {
        match self {
            Self::Http(status) => format!("animation_asset_http_{status}"),
            Self::DecodeFailed(reason) => format!("animation_decode_{reason}"),
            other => match other {
                Self::InvalidReference => "invalid_animation_reference",
                Self::LocalUnavailable => "animation_local_asset_unavailable",
                Self::InvalidPath => "animation_asset_path_rejected",
                Self::TooLarge => "animation_asset_size_limit",
                Self::AuthenticationRequired => "animation_authentication_required",
                Self::Challenge => "animation_account_challenge",
                Self::DeliveryFailed => "animation_delivery_failed",
                Self::InvalidDelivery => "invalid_animation_delivery",
                Self::InvalidRedirect => "unsupported_animation_redirect",
                Self::DownloadFailed => "animation_download_failed",
                Self::Http(_) | Self::DecodeFailed(_) => unreachable!(),
            }
            .into(),
        }
    }
}
impl std::fmt::Display for AssetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.code())
    }
}
impl std::error::Error for AssetError {}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Reference {
    Local(String),
    Roblox(u64),
}
impl Reference {
    pub(crate) fn parse(raw: &str) -> Result<Self, AssetError> {
        if raw.is_empty() || raw.len() > 512 || raw.chars().any(char::is_control) {
            return Err(AssetError::InvalidReference);
        }
        if let Some(path) = raw.strip_prefix("rbxasset://") {
            if path.is_empty()
                || path.contains(['\\', ':', '%', '?', '#'])
                || path
                    .split('/')
                    .any(|p| p.is_empty() || p == "." || p == "..")
                || Path::new(path)
                    .components()
                    .any(|p| !matches!(p, Component::Normal(_)))
            {
                return Err(AssetError::InvalidPath);
            }
            return Ok(Self::Local(path.into()));
        }
        let id = if let Some(id) = raw.strip_prefix("rbxassetid://") {
            id.to_owned()
        } else {
            let url = url::Url::parse(raw).map_err(|_| AssetError::InvalidReference)?;
            if !matches!(url.scheme(), "http" | "https")
                || !matches!(url.host_str(), Some("www.roblox.com" | "roblox.com"))
                || !url.username().is_empty()
                || url.password().is_some()
                || url.port().is_some()
                || url.fragment().is_some()
                || !matches!(url.path(), "/asset" | "/asset/")
            {
                return Err(AssetError::InvalidReference);
            }
            let query = url.query_pairs().collect::<Vec<_>>();
            if query.len() != 1 || query[0].0 != "id" {
                return Err(AssetError::InvalidReference);
            }
            query[0].1.to_string()
        };
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(AssetError::InvalidReference);
        }
        let id = id
            .parse::<u64>()
            .ok()
            .filter(|id| *id > 0)
            .ok_or(AssetError::InvalidReference)?;
        Ok(Self::Roblox(id))
    }
    pub(crate) fn canonical(&self) -> String {
        match self {
            Self::Local(path) => format!("rbxasset://{path}"),
            Self::Roblox(id) => format!("rbxassetid://{id}"),
        }
    }
}

pub trait AnimationBackend: Send + Sync {
    fn load(&self, reference: &str) -> Result<Value, AssetError>;
}

pub fn default_backend() -> std::sync::Arc<dyn AnimationBackend> {
    std::sync::Arc::new(AnimationAssetBackend::local(
        crate::engine_services::local_text::default_content_root(),
    ))
}

pub struct AnimationAssetBackend {
    content_root: Option<PathBuf>,
    account: Option<Mutex<RobloxSession>>,
    cdn: ureq::Agent,
}
impl AnimationAssetBackend {
    pub fn local(content_root: Option<PathBuf>) -> Self {
        Self::new(content_root, None)
    }
    pub fn authenticated(content_root: Option<PathBuf>, account: RobloxSession) -> Self {
        Self::new(content_root, Some(account))
    }
    fn new(content_root: Option<PathBuf>, account: Option<RobloxSession>) -> Self {
        Self {
            content_root: content_root
                .and_then(|p| fs::canonicalize(p).ok())
                .filter(|p| p.is_dir()),
            account: account.map(Mutex::new),
            cdn: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(12))
                .redirects(0)
                .build(),
        }
    }
    fn local_bytes(&self, path: &str) -> Result<Vec<u8>, AssetError> {
        let root = self
            .content_root
            .as_ref()
            .ok_or(AssetError::LocalUnavailable)?;
        let path = fs::canonicalize(root.join(path)).map_err(|_| AssetError::LocalUnavailable)?;
        if !path.starts_with(root) {
            return Err(AssetError::InvalidPath);
        }
        let file = File::open(path).map_err(|_| AssetError::LocalUnavailable)?;
        let info = file.metadata().map_err(|_| AssetError::LocalUnavailable)?;
        if !info.is_file() || info.len() > MAX_BYTES as u64 {
            return Err(AssetError::TooLarge);
        }
        read_bounded(file)
    }
    fn remote_bytes(&self, id: u64) -> Result<Vec<u8>, AssetError> {
        let account = self
            .account
            .as_ref()
            .ok_or(AssetError::AuthenticationRequired)?;
        let response = account
            .lock()
            .map_err(|_| AssetError::DeliveryFailed)?
            .request(
                "assetdelivery.roblox.com",
                &format!("/v2/assetId/{id}"),
                "GET",
                None,
                false,
            )
            .map_err(|_| AssetError::DeliveryFailed)?;
        if response.challenge_present {
            return Err(AssetError::Challenge);
        }
        if response.http_status != 200 {
            return Err(AssetError::Http(response.http_status));
        }
        let location =
            delivery_location(response.data.as_ref().ok_or(AssetError::InvalidDelivery)?)?;
        let mut current = cdn_url(&location)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(12);
        for attempt in 0..=3 {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(AssetError::DownloadFailed);
            }
            // This separate agent has no cookie or account headers, including on redirects.
            let response = match self
                .cdn
                .get(current.as_str())
                .timeout(remaining)
                .set("Accept-Encoding", "identity")
                .call()
            {
                Ok(response) => response,
                Err(ureq::Error::Status(_, response)) => response,
                Err(_) => return Err(AssetError::DownloadFailed),
            };
            match response.status() {
                200 => return read_bounded(response.into_reader()),
                301 | 302 | 303 | 307 | 308 if attempt < 3 => {
                    let next = response
                        .header("Location")
                        .ok_or(AssetError::InvalidRedirect)?;
                    let next = current
                        .join(next)
                        .map_err(|_| AssetError::InvalidRedirect)?;
                    current = cdn_url(next.as_str())?;
                }
                status => return Err(AssetError::Http(status)),
            }
        }
        Err(AssetError::InvalidRedirect)
    }
}
fn read_bounded(reader: impl Read) -> Result<Vec<u8>, AssetError> {
    let mut data = Vec::new();
    reader
        .take((MAX_BYTES + 1) as u64)
        .read_to_end(&mut data)
        .map_err(|_| AssetError::DownloadFailed)?;
    if data.is_empty() || data.len() > MAX_BYTES {
        return Err(AssetError::TooLarge);
    }
    Ok(data)
}
pub(crate) fn delivery_location(data: &Value) -> Result<String, AssetError> {
    if data
        .get("errors")
        .and_then(Value::as_array)
        .is_some_and(|v| !v.is_empty())
    {
        return Err(AssetError::InvalidDelivery);
    }
    let locations = data["locations"]
        .as_array()
        .filter(|a| !a.is_empty() && a.len() <= 8)
        .ok_or(AssetError::InvalidDelivery)?;
    let location = locations[0]["location"]
        .as_str()
        .filter(|s| s.len() <= 4096)
        .ok_or(AssetError::InvalidDelivery)?;
    cdn_url(location)?;
    Ok(location.into())
}
fn cdn_url(raw: &str) -> Result<url::Url, AssetError> {
    let url = url::Url::parse(raw).map_err(|_| AssetError::InvalidRedirect)?;
    let host = url.host_str().ok_or(AssetError::InvalidRedirect)?;
    if url.scheme() != "https"
        || !(host == "rbxcdn.com"
            || host.ends_with(".rbxcdn.com")
            || host == "contentdelivery.roblox.com")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some_and(|p| p != 443)
        || url.fragment().is_some()
        || raw.len() > 4096
    {
        return Err(AssetError::InvalidRedirect);
    }
    Ok(url)
}
impl AnimationBackend for AnimationAssetBackend {
    fn load(&self, reference: &str) -> Result<Value, AssetError> {
        let parsed = Reference::parse(reference)?;
        let raw = match &parsed {
            Reference::Local(path) => self.local_bytes(path)?,
            Reference::Roblox(id) => self.remote_bytes(*id)?,
        };
        let mut clip = animation_codec::decode(&raw).map_err(AssetError::DecodeFailed)?;
        clip["source_sha256"] = json!(format!("{:x}", Sha256::digest(&raw)));
        if let Reference::Roblox(id) = parsed {
            clip["asset_id"] = json!(id.to_string());
        }
        if serde_json::to_vec(&clip)
            .map_err(|_| AssetError::DecodeFailed("serialization_failed"))?
            .len()
            > MAX_CLIP_BYTES
        {
            return Err(AssetError::TooLarge);
        }
        Ok(clip)
    }
}

#[cfg(test)]
#[path = "animation_assets_tests.rs"]
mod tests;
