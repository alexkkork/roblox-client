//! find local assets for the frame we are showing right now.
//! the latest `RenderProjection` has to allow the asset first.
//! only read `rbxasset://` files inside the installed content root.
//! ids and urls do not get a remote fallback.

use crate::render_projection::{RenderAsset, RenderProjection};
use base64::Engine;
use serde_json::{json, Value};
use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::Mutex,
};

const MAX_TEXTURE_BYTES: usize = 8 * 1024 * 1024;
const MAX_MESH_BYTES: usize = 16 * 1024 * 1024;
const MAX_FONT_BYTES: usize = 8 * 1024 * 1024;
const MAX_FAMILY_BYTES: usize = 256 * 1024;
const MAX_DECODED_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_CACHE_ENTRIES: usize = 256;
const MAX_VERTICES: usize = 100_000;
const MAX_FACES: usize = 200_000;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct CacheKey {
    kind: String,
    reference: String,
    weight: Option<u16>,
    style: Option<u8>,
}

struct CacheValue {
    value: Value,
    encoded_bytes: usize,
}

#[derive(Default)]
struct State {
    allowed: BTreeSet<RenderAsset>,
    cache: HashMap<CacheKey, CacheValue>,
    recency: VecDeque<CacheKey>,
    cache_bytes: usize,
}

/// Reads installed assets without network access and validates every response.
pub struct RenderAssetStore {
    content_root: PathBuf,
    max_cache_bytes: usize,
    state: Mutex<State>,
}

#[derive(Debug)]
pub enum RenderAssetError {
    InvalidRequest(&'static str),
    NotReferenced,
    NotLocal,
    Unavailable,
    InvalidAsset(&'static str),
    TooLarge,
    Io(io::Error),
    CachePoisoned,
}

impl std::fmt::Display for RenderAssetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest(reason) => write!(f, "invalid render asset request: {reason}"),
            Self::NotReferenced => f.write_str("asset is not referenced by the displayed frame"),
            Self::NotLocal => f.write_str("asset is not available from installed local content"),
            Self::Unavailable => f.write_str("installed local visual asset is unavailable"),
            Self::InvalidAsset(reason) => write!(f, "invalid local visual asset: {reason}"),
            Self::TooLarge => f.write_str("visual asset exceeds its size budget"),
            Self::Io(_) => f.write_str("cannot read installed local visual asset"),
            Self::CachePoisoned => f.write_str("visual asset cache is unavailable"),
        }
    }
}

impl std::error::Error for RenderAssetError {}

impl RenderAssetStore {
    pub fn new(content_root: impl AsRef<Path>, max_cache_bytes: usize) -> io::Result<Self> {
        let content_root = fs::canonicalize(content_root)?;
        if !content_root.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "content root is not a directory",
            ));
        }
        if !(1024..=256 * 1024 * 1024).contains(&max_cache_bytes) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid asset cache budget",
            ));
        }
        Ok(Self {
            content_root,
            max_cache_bytes,
            state: Mutex::new(State::default()),
        })
    }

    /// Replace the asset gate with references from this exact displayed frame.
    /// Old cached values may remain but cannot be read while not referenced.
    pub fn observe(&self, projection: &RenderProjection) -> Result<(), RenderAssetError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| RenderAssetError::CachePoisoned)?;
        state.allowed.clone_from(projection.assets());
        Ok(())
    }

    /// Resolve one permitted asset to the JSON shape consumed by the UI.
    /// For `font`, reference is the family asset and weight/style are required.
    pub fn get(
        &self,
        kind: &str,
        reference: &str,
        weight: Option<u16>,
        style: Option<u8>,
    ) -> Result<Value, RenderAssetError> {
        let key = make_key(kind, reference, weight, style)?;
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| RenderAssetError::CachePoisoned)?;
            if !is_allowed(&state.allowed, &key) {
                return Err(RenderAssetError::NotReferenced);
            }
            if let Some(value) = state.cache.get(&key).map(|v| v.value.clone()) {
                touch(&mut state.recency, &key);
                return Ok(value);
            }
        }

        let value = match key.kind.as_str() {
            "texture" => self.load_texture(&key.reference)?,
            "mesh" => self.load_mesh(&key.reference)?,
            "font" => self.load_font(&key.reference, key.weight.unwrap(), key.style.unwrap())?,
            _ => return Err(RenderAssetError::InvalidRequest("unsupported kind")),
        };
        let encoded_bytes = serde_json::to_vec(&value)
            .map_err(|_| RenderAssetError::InvalidAsset("response cannot be serialized"))?
            .len();
        if encoded_bytes > MAX_DECODED_RESPONSE_BYTES || encoded_bytes > self.max_cache_bytes {
            return Err(RenderAssetError::TooLarge);
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| RenderAssetError::CachePoisoned)?;
        // The projection may have changed while disk data was being read.
        if !is_allowed(&state.allowed, &key) {
            return Err(RenderAssetError::NotReferenced);
        }
        insert_cache(
            &mut state,
            key,
            CacheValue {
                value: value.clone(),
                encoded_bytes,
            },
            self.max_cache_bytes,
        );
        Ok(value)
    }

    fn load_texture(&self, reference: &str) -> Result<Value, RenderAssetError> {
        let path = self.local_path(reference, AssetKind::Texture)?;
        let raw = read_bounded(&path, MAX_TEXTURE_BYTES)?;
        let (mime, width, height) = image_info(&raw)?;
        if let (Some(width), Some(height)) = (width, height) {
            if width == 0
                || height == 0
                || width > 8192
                || height > 8192
                || width
                    .checked_mul(height)
                    .is_none_or(|pixels| pixels > 16 * 1024 * 1024)
            {
                return Err(RenderAssetError::InvalidAsset(
                    "texture dimensions exceed budget",
                ));
            }
        }
        Ok(
            json!({"ok":true,"mime":mime,"data":base64::engine::general_purpose::STANDARD.encode(raw)}),
        )
    }

    fn load_mesh(&self, reference: &str) -> Result<Value, RenderAssetError> {
        let path = self.local_path(reference, AssetKind::Mesh)?;
        let raw = read_bounded(&path, MAX_MESH_BYTES)?;
        let mesh = decode_mesh(&raw)?.value();
        let value = json!({"ok":true,"mesh":mesh});
        let bytes = serde_json::to_vec(&value)
            .map_err(|_| RenderAssetError::InvalidAsset("mesh serialization failed"))?
            .len();
        if bytes > MAX_DECODED_RESPONSE_BYTES {
            return Err(RenderAssetError::TooLarge);
        }
        Ok(value)
    }

    fn load_font(
        &self,
        reference: &str,
        weight: u16,
        style: u8,
    ) -> Result<Value, RenderAssetError> {
        let family_path = self.local_path(reference, AssetKind::FontFamily)?;
        let family_raw = read_bounded(&family_path, MAX_FAMILY_BYTES)?;
        let faces = crate::engine_services::decode_font_family(&family_raw)
            .map_err(RenderAssetError::InvalidAsset)?;
        let face = crate::engine_services::select_font_face(&faces, weight, style == 1).ok_or(
            RenderAssetError::InvalidAsset("font family has no usable faces"),
        )?;
        let face_ref = face.asset_id.clone();
        let actual_weight = face.weight;
        let actual_style = u8::from(face.italic);
        let font_path = self.local_path(&face_ref, AssetKind::Font)?;
        let raw = read_bounded(&font_path, MAX_FONT_BYTES)?;
        let mime = crate::engine_services::validate_font_bytes(&raw)
            .map_err(RenderAssetError::InvalidAsset)?;
        Ok(
            json!({"ok":true,"mime":mime,"data":base64::engine::general_purpose::STANDARD.encode(raw),
                  "weight":actual_weight,"style":actual_style}),
        )
    }

    fn local_path(&self, reference: &str, kind: AssetKind) -> Result<PathBuf, RenderAssetError> {
        let normalized = normalize_local_reference(reference, kind)?;
        let candidate = self.content_root.join(normalized);
        let path = fs::canonicalize(candidate).map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                RenderAssetError::Unavailable
            } else {
                RenderAssetError::Io(e)
            }
        })?;
        if !path.starts_with(&self.content_root) || !path.is_file() {
            return Err(RenderAssetError::Unavailable);
        }
        Ok(path)
    }
}

#[derive(Clone, Copy)]
enum AssetKind {
    Texture,
    Mesh,
    FontFamily,
    Font,
}

fn normalize_local_reference(reference: &str, kind: AssetKind) -> Result<&str, RenderAssetError> {
    if reference.len() > 512 {
        return Err(RenderAssetError::InvalidRequest("reference too long"));
    }
    let path = reference
        .strip_prefix("rbxasset://")
        .ok_or(RenderAssetError::NotLocal)?;
    let valid = match kind {
        AssetKind::Texture => {
            (path.starts_with("textures/") || path.starts_with("avatar/"))
                && matches!(path.rsplit('.').next(), Some("png" | "jpg" | "jpeg"))
        }
        AssetKind::Mesh => {
            (path.starts_with("textures/") || path.starts_with("avatar/"))
                && path.ends_with(".mesh")
        }
        AssetKind::FontFamily => path.starts_with("fonts/families/") && path.ends_with(".json"),
        AssetKind::Font => {
            path.starts_with("fonts/") && (path.ends_with(".ttf") || path.ends_with(".otf"))
        }
    };
    if !valid
        || path.is_empty()
        || path.contains("..")
        || path.starts_with('/')
        || path
            .bytes()
            .any(|b| !(b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'-' | b'.')))
    {
        return Err(RenderAssetError::NotLocal);
    }
    Ok(path)
}

fn make_key(
    kind: &str,
    reference: &str,
    weight: Option<u16>,
    style: Option<u8>,
) -> Result<CacheKey, RenderAssetError> {
    let asset_kind = match kind {
        "mesh" => AssetKind::Mesh,
        "texture" => AssetKind::Texture,
        "font" => AssetKind::FontFamily,
        _ => return Err(RenderAssetError::InvalidRequest("unsupported kind")),
    };
    let path = normalize_local_reference(reference, asset_kind)?;
    if kind == "font" {
        let weight = weight.ok_or(RenderAssetError::InvalidRequest("font weight required"))?;
        let style = style.ok_or(RenderAssetError::InvalidRequest("font style required"))?;
        if !(100..=900).contains(&weight) || weight % 100 != 0 || style > 1 {
            return Err(RenderAssetError::InvalidRequest("invalid font descriptor"));
        }
        Ok(CacheKey {
            kind: kind.to_owned(),
            reference: format!("rbxasset://{path}"),
            weight: Some(weight),
            style: Some(style),
        })
    } else {
        if weight.is_some() || style.is_some() {
            return Err(RenderAssetError::InvalidRequest(
                "font options require font kind",
            ));
        }
        Ok(CacheKey {
            kind: kind.to_owned(),
            reference: format!("rbxasset://{path}"),
            weight: None,
            style: None,
        })
    }
}

fn is_allowed(allowed: &BTreeSet<RenderAsset>, key: &CacheKey) -> bool {
    allowed.contains(&RenderAsset {
        kind: match key.kind.as_str() {
            "mesh" => "mesh",
            "texture" => "texture",
            _ => "font",
        },
        reference: key.reference.clone(),
        weight: key.weight,
        style: key.style,
    })
}

fn touch(order: &mut VecDeque<CacheKey>, key: &CacheKey) {
    if let Some(index) = order.iter().position(|item| item == key) {
        order.remove(index);
    }
    order.push_back(key.clone());
}

fn insert_cache(state: &mut State, key: CacheKey, value: CacheValue, max_bytes: usize) {
    if let Some(old) = state.cache.remove(&key) {
        state.cache_bytes -= old.encoded_bytes;
    }
    if let Some(index) = state.recency.iter().position(|item| item == &key) {
        state.recency.remove(index);
    }
    while !state.cache.is_empty()
        && (state.cache_bytes + value.encoded_bytes > max_bytes
            || state.cache.len() >= MAX_CACHE_ENTRIES)
    {
        if let Some(old_key) = state.recency.pop_front() {
            if let Some(old) = state.cache.remove(&old_key) {
                state.cache_bytes -= old.encoded_bytes;
            }
        }
    }
    state.cache_bytes += value.encoded_bytes;
    state.recency.push_back(key.clone());
    state.cache.insert(key, value);
}

fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>, RenderAssetError> {
    let file = File::open(path).map_err(RenderAssetError::Io)?;
    let metadata = file.metadata().map_err(RenderAssetError::Io)?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > limit as u64 {
        return Err(RenderAssetError::TooLarge);
    }
    let mut raw = Vec::with_capacity(metadata.len() as usize);
    file.take(limit as u64 + 1)
        .read_to_end(&mut raw)
        .map_err(RenderAssetError::Io)?;
    if raw.is_empty() {
        return Err(RenderAssetError::Unavailable);
    }
    if raw.len() > limit {
        return Err(RenderAssetError::TooLarge);
    }
    Ok(raw)
}

fn image_info(raw: &[u8]) -> Result<(&'static str, Option<u32>, Option<u32>), RenderAssetError> {
    if raw.starts_with(b"\x89PNG\r\n\x1a\n") {
        if raw.len() < 24 || &raw[12..16] != b"IHDR" {
            return Err(RenderAssetError::InvalidAsset("truncated PNG header"));
        }
        let width = u32::from_be_bytes(raw[16..20].try_into().unwrap());
        let height = u32::from_be_bytes(raw[20..24].try_into().unwrap());
        let depth = raw[24];
        let color_type = raw[25];
        let valid_depth = match color_type {
            0 => matches!(depth, 1 | 2 | 4 | 8 | 16),
            2 | 4 | 6 => matches!(depth, 8 | 16),
            3 => matches!(depth, 1 | 2 | 4 | 8),
            _ => false,
        };
        if raw.len() < 33 || !valid_depth || raw[26] != 0 || raw[27] != 0 || raw[28] > 1 {
            return Err(RenderAssetError::InvalidAsset("invalid PNG header"));
        }
        Ok(("image/png", Some(width), Some(height)))
    } else if raw.starts_with(&[0xff, 0xd8]) && raw.ends_with(&[0xff, 0xd9]) {
        Ok(("image/jpeg", None, None))
    } else {
        Err(RenderAssetError::InvalidAsset("unsupported texture format"))
    }
}

struct DecodedMesh {
    positions: Vec<f64>,
    normals: Vec<f64>,
    uv: Vec<f64>,
    indices: Vec<u32>,
    version: String,
    skinned: bool,
}
impl DecodedMesh {
    fn value(self) -> Value {
        json!({"positions":self.positions,"normals":self.normals,"uv":self.uv,"indices":self.indices,"version":self.version,"skinned":self.skinned})
    }
}

fn decode_mesh(raw: &[u8]) -> Result<DecodedMesh, RenderAssetError> {
    if raw.is_empty() || raw.len() > MAX_MESH_BYTES {
        return Err(RenderAssetError::TooLarge);
    }
    if raw.starts_with(&[0x1f, 0x8b]) {
        return Err(RenderAssetError::InvalidAsset(
            "compressed local mesh format is unsupported",
        ));
    }
    let newline = raw
        .iter()
        .position(|b| *b == b'\n')
        .ok_or(RenderAssetError::InvalidAsset("mesh header missing"))?;
    // Legacy text meshes may end their header line with CRLF.
    let version = std::str::from_utf8(&raw[..newline])
        .map_err(|_| RenderAssetError::InvalidAsset("invalid mesh header"))?
        .trim_end_matches('\r');
    let body = &raw[newline + 1..];
    match version {
        "version 1.00" | "version 1.01" => decode_mesh_v1(version, body),
        "version 2.00" | "version 3.00" | "version 3.01" | "version 4.00" | "version 4.01"
        | "version 5.00" => decode_mesh_binary(version, body),
        // Chunked meshes; 7.00 stores its geometry with Draco (draco_mesh.rs).
        "version 6.00" | "version 7.00" => {
            let mesh = crate::draco_mesh::decode_chunked(body)
                .map_err(|_| RenderAssetError::InvalidAsset("invalid chunked mesh"))?;
            if mesh.positions.iter().chain(&mesh.normals).chain(&mesh.uvs).any(|v| !v.is_finite()) {
                return Err(RenderAssetError::InvalidAsset("mesh vertex is not finite"));
            }
            Ok(DecodedMesh {
                positions: mesh.positions,
                normals: mesh.normals,
                uv: mesh.uvs,
                indices: mesh.indices,
                version: version.to_owned(),
                skinned: false,
            })
        }
        _ => Err(RenderAssetError::InvalidAsset("unsupported mesh version")),
    }
}

fn decode_mesh_v1(version: &str, body: &[u8]) -> Result<DecodedMesh, RenderAssetError> {
    let newline = body
        .iter()
        .position(|b| *b == b'\n')
        .ok_or(RenderAssetError::InvalidAsset("mesh face count missing"))?;
    let face_count = std::str::from_utf8(&body[..newline])
        .ok()
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|n| *n > 0 && *n <= MAX_FACES)
        .ok_or(RenderAssetError::InvalidAsset("invalid mesh face count"))?;
    let vectors = parse_vectors(&body[newline + 1..])?;
    if vectors.len()
        != face_count
            .checked_mul(9)
            .ok_or(RenderAssetError::TooLarge)?
    {
        return Err(RenderAssetError::InvalidAsset(
            "mesh vertex data count mismatch",
        ));
    }
    let mut positions = Vec::with_capacity(face_count * 9);
    let mut normals = Vec::with_capacity(face_count * 9);
    let mut uv = Vec::with_capacity(face_count * 6);
    let mut indices = Vec::with_capacity(face_count * 3);
    for face in 0..face_count {
        for corner in 0..3 {
            let base = face * 9 + corner * 3;
            let mut p = vectors[base];
            if version == "version 1.00" {
                for n in &mut p {
                    *n *= 0.5;
                }
            }
            positions.extend(p);
            normals.extend(vectors[base + 1]);
            uv.extend([
                vectors[base + 2][0],
                if version == "version 1.00" {
                    1.0 - vectors[base + 2][1]
                } else {
                    vectors[base + 2][1]
                },
            ]);
            indices.push((face * 3 + corner) as u32);
        }
    }
    finish_mesh(version, positions, normals, uv, indices, false)
}

fn parse_vectors(raw: &[u8]) -> Result<Vec<[f64; 3]>, RenderAssetError> {
    let text = std::str::from_utf8(raw)
        .map_err(|_| RenderAssetError::InvalidAsset("mesh text is not UTF-8"))?;
    let mut rest = text;
    let mut result = Vec::new();
    while let Some(open) = rest.find('[') {
        if !rest[..open].trim().is_empty() {
            return Err(RenderAssetError::InvalidAsset("unexpected mesh text"));
        }
        let after = &rest[open + 1..];
        let close = after
            .find(']')
            .ok_or(RenderAssetError::InvalidAsset("truncated mesh vector"))?;
        let parts: Vec<_> = after[..close].split(',').map(str::trim).collect();
        if parts.len() != 3 {
            return Err(RenderAssetError::InvalidAsset("invalid mesh vector"));
        }
        let mut values = [0.0; 3];
        for (index, token) in parts.iter().enumerate() {
            values[index] = token
                .parse::<f64>()
                .map_err(|_| RenderAssetError::InvalidAsset("invalid mesh number"))?;
            if !values[index].is_finite() || values[index].abs() > 1e8 {
                return Err(RenderAssetError::InvalidAsset(
                    "mesh coordinate out of range",
                ));
            }
        }
        result.push(values);
        if result.len() > MAX_FACES * 9 {
            return Err(RenderAssetError::TooLarge);
        }
        rest = &after[close + 1..];
    }
    if !rest.trim().is_empty() {
        return Err(RenderAssetError::InvalidAsset("unexpected mesh text"));
    }
    Ok(result)
}

fn decode_mesh_v2(body: &[u8], version: &str) -> Result<DecodedMesh, RenderAssetError> {
    if body.len() < 12 {
        return Err(RenderAssetError::InvalidAsset("truncated v2 mesh header"));
    }
    let header = le_u16(body, 0)? as usize;
    let stride = body[2] as usize;
    let face_stride = body[3] as usize;
    let vertices = le_u32(body, 4)? as usize;
    let faces = le_u32(body, 8)? as usize;
    if header != 12 || !matches!(stride, 36 | 40) || face_stride != 12 {
        return Err(RenderAssetError::InvalidAsset("invalid v2 mesh header"));
    }
    decode_mesh_records(version, body, header, stride, vertices, faces, 0, false, 0)
}

fn decode_mesh_binary(version: &str, body: &[u8]) -> Result<DecodedMesh, RenderAssetError> {
    if version == "version 2.00" {
        return decode_mesh_v2(body, version);
    }
    let (header, stride, vertices, faces, lods, tail, skinned) = match version {
        "version 3.00" | "version 3.01" => {
            if body.len() < 16 {
                return Err(RenderAssetError::InvalidAsset("truncated v3 mesh header"));
            }
            let header = le_u16(body, 0)? as usize;
            let stride = body[2] as usize;
            let face_stride = body[3] as usize;
            let lod_size = le_u16(body, 4)? as usize;
            let lods = le_u16(body, 6)? as usize;
            if header != 16 || !matches!(stride, 36 | 40) || face_stride != 12 || lod_size != 4 {
                return Err(RenderAssetError::InvalidAsset("invalid v3 mesh header"));
            }
            (
                header,
                stride,
                le_u32(body, 8)? as usize,
                le_u32(body, 12)? as usize,
                lods,
                0,
                false,
            )
        }
        "version 4.00" | "version 4.01" | "version 5.00" => {
            if body.len() < 24 {
                return Err(RenderAssetError::InvalidAsset(
                    "truncated modern mesh header",
                ));
            }
            let header = le_u16(body, 0)? as usize;
            let vertices = le_u32(body, 4)? as usize;
            let faces = le_u32(body, 8)? as usize;
            let lods = le_u16(body, 12)? as usize;
            let bones = le_u16(body, 14)? as usize;
            let names = le_u32(body, 16)? as usize;
            let subsets = le_u16(body, 20)? as usize;
            let expected = if version == "version 5.00" { 32 } else { 24 };
            if header != expected || body.len() < header {
                return Err(RenderAssetError::InvalidAsset("invalid modern mesh header"));
            }
            let mut tail = bones
                .checked_mul(60)
                .and_then(|n| n.checked_add(names))
                .and_then(|n| n.checked_add(subsets * 72))
                .ok_or(RenderAssetError::TooLarge)?;
            if header == 32 {
                tail = tail
                    .checked_add(le_u32(body, 28)? as usize)
                    .ok_or(RenderAssetError::TooLarge)?;
            }
            (header, 40, vertices, faces, lods, tail, bones > 0)
        }
        _ => return Err(RenderAssetError::InvalidAsset("unsupported mesh version")),
    };
    if lods > 256 {
        return Err(RenderAssetError::InvalidAsset("mesh LOD budget exceeded"));
    }
    decode_mesh_records(
        version, body, header, stride, vertices, faces, lods, skinned, tail,
    )
}

fn decode_mesh_records(
    version: &str,
    body: &[u8],
    header: usize,
    stride: usize,
    vertices: usize,
    faces: usize,
    lods: usize,
    skinned: bool,
    tail: usize,
) -> Result<DecodedMesh, RenderAssetError> {
    if vertices == 0 || vertices > MAX_VERTICES || faces == 0 || faces > MAX_FACES {
        return Err(RenderAssetError::InvalidAsset(
            "mesh geometry budget exceeded",
        ));
    }
    let bone_weights = if skinned {
        vertices.checked_mul(8).ok_or(RenderAssetError::TooLarge)?
    } else {
        0
    };
    let face_start = header
        .checked_add(
            vertices
                .checked_mul(stride)
                .ok_or(RenderAssetError::TooLarge)?,
        )
        .and_then(|n| n.checked_add(bone_weights))
        .ok_or(RenderAssetError::TooLarge)?;
    let lod_start = face_start
        .checked_add(faces.checked_mul(12).ok_or(RenderAssetError::TooLarge)?)
        .ok_or(RenderAssetError::TooLarge)?;
    let expected = lod_start
        .checked_add(lods.checked_mul(4).ok_or(RenderAssetError::TooLarge)?)
        .and_then(|n| n.checked_add(tail))
        .ok_or(RenderAssetError::TooLarge)?;
    if body.len() != expected {
        return Err(RenderAssetError::InvalidAsset(
            "truncated or trailing mesh bytes",
        ));
    }
    let (mut start, mut end) = (0, faces);
    if lods > 0 {
        let offsets: Vec<usize> = (0..lods)
            .map(|i| le_u32(body, lod_start + i * 4).map(|n| n as usize))
            .collect::<Result<_, _>>()?;
        if offsets[0] != 0
            || offsets.windows(2).any(|w| w[0] > w[1])
            || *offsets.last().unwrap() > faces
        {
            return Err(RenderAssetError::InvalidAsset("invalid mesh LOD ranges"));
        }
        // Draw the first non-empty level; a table of empty levels (dynamic
        // heads store 0, 0) means one level of every face.
        if let Some(level) = offsets.windows(2).find(|w| w[0] < w[1]) {
            (start, end) = (level[0], level[1]);
        }
    }
    let mut positions = Vec::with_capacity(vertices * 3);
    let mut normals = Vec::with_capacity(vertices * 3);
    let mut uv = Vec::with_capacity(vertices * 2);
    for i in 0..vertices {
        let off = header + i * stride;
        for c in 0..3 {
            positions.push(f32_at(body, off + c * 4)? as f64);
        }
        for c in 0..3 {
            normals.push(f32_at(body, off + 12 + c * 4)? as f64);
        }
        for c in 0..2 {
            uv.push(f32_at(body, off + 24 + c * 4)? as f64);
        }
    }
    let mut indices = Vec::with_capacity((end - start) * 3);
    for i in start..end {
        for c in 0..3 {
            let index = le_u32(body, face_start + i * 12 + c * 4)?;
            if index as usize >= vertices {
                return Err(RenderAssetError::InvalidAsset("mesh index out of range"));
            }
            indices.push(index);
        }
    }
    finish_mesh(version, positions, normals, uv, indices, skinned)
}

fn finish_mesh(
    version: &str,
    positions: Vec<f64>,
    normals: Vec<f64>,
    uv: Vec<f64>,
    indices: Vec<u32>,
    skinned: bool,
) -> Result<DecodedMesh, RenderAssetError> {
    if indices.is_empty()
        || positions
            .iter()
            .chain(&normals)
            .chain(&uv)
            .any(|n| !n.is_finite() || n.abs() > 1e8)
    {
        return Err(RenderAssetError::InvalidAsset("invalid decoded mesh"));
    }
    Ok(DecodedMesh {
        positions,
        normals,
        uv,
        indices,
        version: version.to_owned(),
        skinned,
    })
}
fn le_u16(raw: &[u8], i: usize) -> Result<u16, RenderAssetError> {
    Ok(u16::from_le_bytes(
        raw.get(i..i + 2)
            .ok_or(RenderAssetError::InvalidAsset("truncated mesh field"))?
            .try_into()
            .unwrap(),
    ))
}
fn le_u32(raw: &[u8], i: usize) -> Result<u32, RenderAssetError> {
    Ok(u32::from_le_bytes(
        raw.get(i..i + 4)
            .ok_or(RenderAssetError::InvalidAsset("truncated mesh field"))?
            .try_into()
            .unwrap(),
    ))
}
fn f32_at(raw: &[u8], i: usize) -> Result<f64, RenderAssetError> {
    Ok(f32::from_le_bytes(
        raw.get(i..i + 4)
            .ok_or(RenderAssetError::InvalidAsset("truncated mesh vertex"))?
            .try_into()
            .unwrap(),
    ) as f64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_projection::project_frame;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    struct TempRoot(PathBuf);
    impl TempRoot {
        fn new() -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir()
                .join(format!("rove-render-assets-{}-{nonce}", std::process::id()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn one_texture_frame(ref_path: &str) -> RenderProjection {
        project_frame(json!({"version":2,"sequence":1,"parts":[{"id":"p","class":"MeshPart","mesh":{"id":"","texture":""},"appearance":{"colorMap":ref_path}}],"gui":[],"players":[]})).unwrap()
    }

    #[test]
    fn image_is_only_read_under_current_frame_gate_and_uses_existing_response_shape() {
        let root = TempRoot::new();
        fs::create_dir_all(root.0.join("textures")).unwrap();
        let mut png = vec![0u8; 33];
        png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        png[12..16].copy_from_slice(b"IHDR");
        png[16..20].copy_from_slice(&2u32.to_be_bytes());
        png[20..24].copy_from_slice(&3u32.to_be_bytes());
        png[24] = 8;
        png[25] = 2;
        png[26] = 0;
        png[27] = 0;
        png[28] = 0;
        fs::write(root.0.join("textures/a.png"), png.clone()).unwrap();
        let store = RenderAssetStore::new(&root.0, 1024 * 1024).unwrap();
        store
            .observe(&one_texture_frame("rbxasset://textures/a.png"))
            .unwrap();
        let response = store
            .get("texture", "rbxasset://textures/a.png", None, None)
            .unwrap();
        assert_eq!(response["ok"], true);
        assert_eq!(response["mime"], "image/png");
        assert_eq!(
            base64::engine::general_purpose::STANDARD
                .decode(response["data"].as_str().unwrap())
                .unwrap(),
            png
        );
        assert!(matches!(
            store.get("texture", "rbxasset://textures/missing.png", None, None),
            Err(RenderAssetError::NotReferenced)
        ));
        store
            .observe(&one_texture_frame("rbxasset://textures/other.png"))
            .unwrap();
        assert!(matches!(
            store.get("texture", "rbxasset://textures/a.png", None, None),
            Err(RenderAssetError::NotReferenced)
        ));
    }

    #[test]
    fn refuses_symlink_escape_and_asset_id_remote_fallback() {
        let root = TempRoot::new();
        fs::create_dir_all(root.0.join("textures")).unwrap();
        let outside = TempRoot::new();
        fs::write(outside.0.join("secret.png"), b"secret").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(
            outside.0.join("secret.png"),
            root.0.join("textures/link.png"),
        )
        .unwrap();
        let store = RenderAssetStore::new(&root.0, 1024 * 1024).unwrap();
        store
            .observe(&one_texture_frame("rbxasset://textures/link.png"))
            .unwrap();
        assert!(matches!(
            store.get("texture", "rbxasset://textures/link.png", None, None),
            Err(RenderAssetError::Unavailable)
        ));
        store
            .observe(&one_texture_frame("rbxassetid://123"))
            .unwrap();
        assert!(matches!(
            store.get("texture", "rbxassetid://123", None, None),
            Err(RenderAssetError::NotLocal)
        ));
    }

    #[test]
    fn decodes_v1_visual_mesh_and_v2_binary_mesh() {
        let v1 =
            b"version 1.01\n1\n[0,0,0][0,0,1][0,0,0][1,0,0][0,0,1][1,0,0][0,1,0][0,0,1][0,1,1]";
        let mesh = decode_mesh(v1).unwrap();
        assert_eq!(mesh.positions.len(), 9);
        assert_eq!(mesh.normals.len(), 9);
        assert_eq!(mesh.uv.len(), 6);
        assert_eq!(mesh.indices, vec![0, 1, 2]);
        let mut v2 = b"version 2.00\n".to_vec();
        v2.extend_from_slice(&12u16.to_le_bytes());
        v2.extend_from_slice(&36u8.to_le_bytes());
        v2.push(12);
        v2.extend_from_slice(&3u32.to_le_bytes());
        v2.extend_from_slice(&1u32.to_le_bytes());
        for vertex in [
            [0f32, 0., 0., 0., 0., 1., 0., 0.],
            [1., 0., 0., 0., 0., 1., 1., 0.],
            [0., 1., 0., 0., 0., 1., 0., 1.],
        ] {
            for n in vertex {
                v2.extend_from_slice(&n.to_le_bytes());
            }
            v2.extend_from_slice(&[0; 4]);
        }
        for i in [0u32, 1, 2] {
            v2.extend_from_slice(&i.to_le_bytes());
        }
        let mesh = decode_mesh(&v2).unwrap();
        assert_eq!(mesh.version, "version 2.00");
        assert_eq!(mesh.positions.len(), 9);
        assert_eq!(mesh.indices, vec![0, 1, 2]);
    }

    #[test]
    fn decodes_v3_v4_and_v5_binary_mesh_headers() {
        fn vertex_block(stride: usize) -> Vec<u8> {
            let mut bytes = Vec::new();
            for vertex in [
                [0f32, 0., 0., 0., 0., 1., 0., 0.],
                [1., 0., 0., 0., 0., 1., 1., 0.],
                [0., 1., 0., 0., 0., 1., 0., 1.],
            ] {
                for n in vertex {
                    bytes.extend_from_slice(&n.to_le_bytes());
                }
                bytes.resize(bytes.len() + stride - 32, 0);
            }
            for i in [0u32, 1, 2] {
                bytes.extend_from_slice(&i.to_le_bytes());
            }
            bytes
        }
        let mut v3 = b"version 3.00\n".to_vec();
        v3.extend_from_slice(&16u16.to_le_bytes());
        v3.extend_from_slice(&36u8.to_le_bytes());
        v3.push(12);
        v3.extend_from_slice(&4u16.to_le_bytes());
        v3.extend_from_slice(&0u16.to_le_bytes());
        v3.extend_from_slice(&3u32.to_le_bytes());
        v3.extend_from_slice(&1u32.to_le_bytes());
        v3.extend(vertex_block(36));
        assert_eq!(decode_mesh(&v3).unwrap().indices, vec![0, 1, 2]);

        let mut v4 = b"version 4.00\n".to_vec();
        v4.extend_from_slice(&24u16.to_le_bytes());
        v4.extend_from_slice(&0u16.to_le_bytes());
        v4.extend_from_slice(&3u32.to_le_bytes());
        v4.extend_from_slice(&1u32.to_le_bytes());
        v4.extend_from_slice(&0u16.to_le_bytes());
        v4.extend_from_slice(&0u16.to_le_bytes());
        v4.extend_from_slice(&0u32.to_le_bytes());
        v4.extend_from_slice(&0u16.to_le_bytes());
        v4.extend_from_slice(&[0, 0]);
        v4.extend(vertex_block(40));
        assert_eq!(decode_mesh(&v4).unwrap().version, "version 4.00");

        let mut v5 = b"version 5.00\n".to_vec();
        v5.extend_from_slice(&32u16.to_le_bytes());
        v5.extend_from_slice(&0u16.to_le_bytes());
        v5.extend_from_slice(&3u32.to_le_bytes());
        v5.extend_from_slice(&1u32.to_le_bytes());
        v5.extend_from_slice(&0u16.to_le_bytes());
        v5.extend_from_slice(&0u16.to_le_bytes());
        v5.extend_from_slice(&0u32.to_le_bytes());
        v5.extend_from_slice(&0u16.to_le_bytes());
        v5.extend_from_slice(&[0, 0]);
        v5.extend_from_slice(&0u32.to_le_bytes());
        v5.extend_from_slice(&0u32.to_le_bytes());
        v5.extend(vertex_block(40));
        assert_eq!(decode_mesh(&v5).unwrap().version, "version 5.00");
    }

    #[test]
    fn selects_font_face_and_validates_bounded_sfnt_directory() {
        let root = TempRoot::new();
        fs::create_dir_all(root.0.join("fonts/families")).unwrap();
        fs::create_dir_all(root.0.join("fonts")).unwrap();
        fs::write(
            root.0.join("fonts/families/f.json"),
            br#"{"faces":[{"assetId":"rbxasset://fonts/a.ttf","weight":400,"style":"normal"}]}"#,
        )
        .unwrap();
        let mut font = vec![0u8; 28];
        font[..4].copy_from_slice(&[0, 1, 0, 0]);
        font[4..6].copy_from_slice(&1u16.to_be_bytes());
        font[12..16].copy_from_slice(b"head");
        font[20..24].copy_from_slice(&28u32.to_be_bytes());
        font[24..28].copy_from_slice(&0u32.to_be_bytes());
        fs::write(root.0.join("fonts/a.ttf"), font).unwrap();
        let projection=project_frame(json!({"version":2,"sequence":1,"parts":[],"players":[],"gui":[{"id":"g","class":"TextLabel","fontFace":{"family":"rbxasset://fonts/families/f.json","weight":400,"style":0}}]})).unwrap();
        let store = RenderAssetStore::new(&root.0, 1024 * 1024).unwrap();
        store.observe(&projection).unwrap();
        let result = store
            .get(
                "font",
                "rbxasset://fonts/families/f.json",
                Some(400),
                Some(0),
            )
            .unwrap();
        assert_eq!(result["mime"], "font/ttf");
        assert_eq!(result["weight"], 400);
        assert_eq!(result["style"], 0);
    }
}
