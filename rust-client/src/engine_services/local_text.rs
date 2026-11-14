//! Offline font lookup and portable Rust text shaping.
//!
//! Only requested faces under the local content root are opened. Font bytes
//! and the number of retained faces are bounded; missing assets remain errors.

use crate::engine_services::{
    decode_font_family, select_font_face, validate_font_bytes, FontFace, TextRequest,
    TextServiceBackend, MAX_FONT_BYTES, MAX_FONT_FAMILY_BYTES,
};
use harfrust::Font;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
    sync::Mutex,
    time::SystemTime,
};
use unicode_segmentation::UnicodeSegmentation;

const MAX_TEXT_BYTES: usize = 4096;
const MAX_SHAPING_WORK: usize = 2_000_000;
const MAX_RUNS: usize = 128;
const MAX_VARIANTS: usize = 16;
const MAX_CACHED_FONT_BYTES: usize = 2 * 1024 * 1024;
const MAX_CACHED_FONTS: usize = 8;
const MAX_REQUEST_FONT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone)]
struct LoadedFont {
    font: Font,
    bytes: usize,
}

#[derive(Clone)]
struct FontSpec {
    family: String,
    weight: u16,
    italic: bool,
    size: f64,
    loaded: LoadedFont,
}

#[derive(Clone)]
struct Run {
    start: usize,
    end: usize,
    spec: FontSpec,
}

struct CachedFont {
    path: PathBuf,
    modified: Option<SystemTime>,
    loaded: LoadedFont,
}

#[derive(Default)]
struct FontCache {
    entries: VecDeque<CachedFont>,
    bytes: usize,
}

impl FontCache {
    fn get(&mut self, path: &Path, size: u64, modified: Option<SystemTime>) -> Option<LoadedFont> {
        let index = self.entries.iter().position(|e| e.path == path)?;
        let entry = self.entries.remove(index)?;
        self.bytes -= entry.loaded.bytes;
        if modified.is_none() || entry.modified != modified || entry.loaded.bytes as u64 != size {
            return None;
        }
        let loaded = entry.loaded.clone();
        self.bytes += entry.loaded.bytes;
        self.entries.push_back(entry);
        Some(loaded)
    }

    fn insert(&mut self, entry: CachedFont) {
        if entry.loaded.bytes > MAX_CACHED_FONT_BYTES {
            return;
        }
        if let Some(index) = self.entries.iter().position(|e| e.path == entry.path) {
            self.bytes -= self.entries.remove(index).unwrap().loaded.bytes;
        }
        while self.entries.len() >= MAX_CACHED_FONTS
            || self.bytes + entry.loaded.bytes > MAX_CACHED_FONT_BYTES
        {
            let Some(old) = self.entries.pop_front() else {
                break;
            };
            self.bytes -= old.loaded.bytes;
        }
        self.bytes += entry.loaded.bytes;
        self.entries.push_back(entry);
    }
}

pub struct LocalTextBackend {
    content_root: PathBuf,
    fonts: Mutex<FontCache>,
}

/// Discover installed content without consulting another user's profile.
/// The explicit environment override also works on Linux and test runners.
pub fn default_content_root() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("ROBLOX_CONTENT_DIR") {
        return Some(path.into());
    }
    #[cfg(target_os = "macos")]
    {
        let path = PathBuf::from("/Applications/Roblox.app/Contents/Resources/content");
        path.is_dir().then_some(path)
    }
    #[cfg(windows)]
    {
        let versions = PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("Roblox/Versions");
        let mut candidates: Vec<_> = fs::read_dir(versions)
            .ok()?
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("content"))
            .filter(|path| path.join("fonts/families").is_dir())
            .collect();
        candidates.sort_by_key(|path| fs::metadata(path).and_then(|m| m.modified()).ok());
        candidates.pop()
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        None
    }
}

impl Default for LocalTextBackend {
    fn default() -> Self {
        Self::new(default_content_root().unwrap_or_default())
    }
}

fn read_bounded(path: &Path, max: usize) -> Result<Vec<u8>, String> {
    let file = File::open(path).map_err(|_| "Installed Roblox font asset is unavailable")?;
    let metadata = file
        .metadata()
        .map_err(|_| "Installed Roblox font asset is unavailable")?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > max as u64 {
        return Err("Invalid or oversized font asset".into());
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(max as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Installed Roblox font asset could not be read")?;
    if bytes.is_empty() || bytes.len() > max {
        return Err("Invalid or oversized font asset".into());
    }
    Ok(bytes)
}

impl LocalTextBackend {
    pub fn new(content_root: impl Into<PathBuf>) -> Self {
        Self {
            content_root: content_root.into(),
            fonts: Mutex::new(FontCache::default()),
        }
    }

    fn checked_local_path(&self, suffix: &str) -> Result<PathBuf, String> {
        if suffix.len() > 512
            || suffix.contains(['\0', '\\', ':'])
            || suffix
                .split('/')
                .any(|part| part.is_empty() || part == ".." || part == ".")
        {
            return Err("Invalid font asset reference".into());
        }
        if self.content_root.as_os_str().is_empty() {
            return Err(
                "Installed Roblox font assets are unavailable; set ROBLOX_CONTENT_DIR".into(),
            );
        }
        let root = self
            .content_root
            .canonicalize()
            .map_err(|_| "Installed Roblox font assets are unavailable")?;
        let path = root
            .join(suffix)
            .canonicalize()
            .map_err(|_| "Installed Roblox font asset is unavailable")?;
        if !path.starts_with(&root) || !path.is_file() {
            return Err("Installed Roblox font asset is unavailable".into());
        }
        Ok(path)
    }

    fn family_data(&self, reference: &str) -> Result<(String, Vec<FontFace>, PathBuf), String> {
        let suffix = reference
            .strip_prefix("rbxasset://fonts/families/")
            .ok_or("Font family is not installed locally")?;
        let path = self.checked_local_path(&format!("fonts/families/{suffix}"))?;
        let raw = read_bounded(&path, MAX_FONT_FAMILY_BYTES)?;
        let value: Value = serde_json::from_slice(&raw).map_err(|_| "Invalid font family JSON")?;
        let family_name = value
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty() && name.len() <= 256)
            .ok_or("Font family name is unavailable")?
            .to_owned();
        let faces = decode_font_family(&raw).map_err(str::to_owned)?;
        Ok((family_name, faces, path))
    }

    fn font_path(&self, family: &str, face: &FontFace) -> Result<PathBuf, String> {
        if let Some(path) = face.asset_id.strip_prefix("rbxasset://") {
            return self.checked_local_path(path);
        }
        let safe = |value: &str| {
            value
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                .collect::<String>()
        };
        let family = safe(family);
        let name = safe(
            face.name
                .as_deref()
                .ok_or("Font family face name is unavailable")?,
        );
        if family.is_empty() || name.is_empty() {
            return Err("Installed Roblox font face is unavailable".into());
        }
        for extension in ["ttf", "otf"] {
            if let Ok(path) = self.checked_local_path(&format!("fonts/{family}-{name}.{extension}"))
            {
                return Ok(path);
            }
        }
        Err("Installed Roblox font face is unavailable".into())
    }

    fn load_font(&self, path: &Path) -> Result<LoadedFont, String> {
        let metadata =
            fs::metadata(path).map_err(|_| "Installed Roblox font face is unavailable")?;
        if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_FONT_BYTES as u64 {
            return Err("Invalid or oversized font file".into());
        }
        let modified = metadata.modified().ok();
        if let Some(font) = self
            .fonts
            .lock()
            .map_err(|_| "Font cache unavailable")?
            .get(path, metadata.len(), modified)
        {
            return Ok(font);
        }
        let raw = read_bounded(path, MAX_FONT_BYTES)?;
        validate_font_bytes(&raw).map_err(str::to_owned)?;
        let bytes = raw.len();
        let font = Font::new(raw, 0).ok_or("Font shaping engine could not open this font")?;
        if font.units_per_em() == 0 || font.num_glyphs() == 0 {
            return Err("Font is missing required metrics".into());
        }
        let loaded = LoadedFont { font, bytes };
        self.fonts
            .lock()
            .map_err(|_| "Font cache unavailable")?
            .insert(CachedFont {
                path: path.to_owned(),
                modified,
                loaded: loaded.clone(),
            });
        Ok(loaded)
    }

    fn measure(&self, spec: &FontSpec, text: &str) -> Result<f64, String> {
        super::text_shaping::advance(&spec.loaded.font, text, spec.size)
    }

    fn bounds(&self, request: &Value) -> Result<Value, String> {
        let source = request
            .get("text")
            .and_then(Value::as_str)
            .ok_or("Invalid text measurement parameters")?;
        if source.len() > MAX_TEXT_BYTES || source.contains('\0') {
            return Err("Invalid text measurement parameters".into());
        }
        let base_size = finite_number(request.get("size"))
            .filter(|n| (1.0..=1000.0).contains(n))
            .ok_or("Invalid text measurement parameters")?;
        // GUI TextScaled scales the entire styled line, including explicit
        // rich-text sizes. Keep original run sizes intact until shaping.
        let layout_scale = match request.get("layout_scale") {
            None => 1.0,
            value => finite_number(value)
                .filter(|n| (0.001..=1000.0).contains(n))
                .ok_or("Invalid text layout scale")?,
        };
        let size = base_size * layout_scale;
        let width = finite_number(request.get("width"))
            .filter(|n| (0.0..=32768.0).contains(n))
            .ok_or("Invalid text measurement parameters")?;
        let line_spacing = match request.get("line_height") {
            None => 1.0,
            value => finite_number(value)
                .filter(|n| (1.0..=3.0).contains(n))
                .ok_or("Invalid text line height")?,
        };
        if request.get("rich_text").and_then(Value::as_bool) == Some(true) {
            return Err("Rich text must be parsed by the native worker".into());
        }
        let base = request
            .get("font")
            .and_then(Value::as_object)
            .ok_or("Invalid displayed font descriptor")?;
        let family = base
            .get("family")
            .and_then(Value::as_str)
            .ok_or("Invalid displayed font descriptor")?;
        let weight = base
            .get("weight")
            .and_then(Value::as_u64)
            .filter(|w| (100..=900).contains(w) && w % 100 == 0)
            .ok_or("Invalid displayed font weight or style")? as u16;
        let style = base
            .get("style")
            .and_then(Value::as_u64)
            .filter(|s| *s <= 1)
            .ok_or("Invalid displayed font weight or style")?
            == 1;
        if base.len() != 3
            || !base.contains_key("family")
            || !base.contains_key("weight")
            || !base.contains_key("style")
        {
            return Err("Invalid displayed font descriptor".into());
        }
        let (mut text, runs) = self.make_runs(
            source,
            base_size,
            family,
            weight,
            style,
            layout_scale,
            request.get("runs"),
        )?;
        if text.is_empty() {
            return Ok(json!([0, 0]));
        }
        let variants: std::collections::HashSet<(String, u16, bool, u64)> = runs
            .iter()
            .map(|r| {
                (
                    r.spec.family.clone(),
                    r.spec.weight,
                    r.spec.italic,
                    r.spec.size.to_bits(),
                )
            })
            .collect();
        if variants.len() > MAX_VARIANTS {
            return Err("Text measurement font variant limit exceeded".into());
        }
        let mut measured: HashMap<(usize, usize, usize), f64> = HashMap::new();
        let mut work = 0usize;
        let advance = |start: usize,
                       end: usize,
                       measured: &mut HashMap<(usize, usize, usize), f64>,
                       work: &mut usize|
         -> Result<f64, String> {
            let mut total = 0.0;
            for (idx, run) in runs.iter().enumerate() {
                let a = start.max(run.start);
                let b = end.min(run.end);
                if a >= b {
                    continue;
                }
                let key = (idx, a, b);
                if let Some(value) = measured.get(&key) {
                    total += value;
                    continue;
                }
                let value_text = &text[a..b];
                *work += value_text.chars().count();
                if *work > MAX_SHAPING_WORK {
                    return Err("Text shaping work limit exceeded".into());
                }
                let value = self.measure(&run.spec, value_text)?;
                measured.insert(key, value);
                total += value;
            }
            Ok(total)
        };
        let mut widest: f64 = 0.0;
        let mut total_height: f64 = 0.0;
        let mut last_height: f64 = 0.0;
        let mut paragraph_start = 0usize;
        loop {
            let paragraph_end = text[paragraph_start..]
                .find('\n')
                .map(|i| paragraph_start + i)
                .unwrap_or(text.len());
            if width == 0.0
                || advance(paragraph_start, paragraph_end, &mut measured, &mut work)? <= width
            {
                widest = widest.max(advance(
                    paragraph_start,
                    paragraph_end,
                    &mut measured,
                    &mut work,
                )?);
                last_height = line_height(&runs, paragraph_start, paragraph_end, size);
                total_height += last_height * line_spacing;
            } else {
                let paragraph = &text[paragraph_start..paragraph_end];
                let graphemes: Vec<(usize, &str)> =
                    UnicodeSegmentation::grapheme_indices(paragraph, true).collect();
                let mut positions = Vec::with_capacity(graphemes.len() + 1);
                positions.push(paragraph_start);
                for (offset, cluster) in &graphemes {
                    positions.push(paragraph_start + offset + cluster.len());
                }
                let opportunities: std::collections::BTreeSet<_> =
                    unicode_linebreak::linebreaks(paragraph)
                        .map(|(offset, _)| paragraph_start + offset)
                        .collect();
                let mut start = 0usize;
                while start < graphemes.len() {
                    let remaining = graphemes.len() - start;
                    let mut low = 0usize;
                    let mut high = 1usize;
                    while high <= remaining
                        && advance(
                            positions[start],
                            positions[start + high],
                            &mut measured,
                            &mut work,
                        )? <= width
                    {
                        low = high;
                        high *= 2;
                    }
                    high = high.min(remaining);
                    while low < high {
                        let middle = (low + high + 1) / 2;
                        if advance(
                            positions[start],
                            positions[start + middle],
                            &mut measured,
                            &mut work,
                        )? <= width
                        {
                            low = middle;
                        } else {
                            high = middle - 1;
                        }
                    }
                    let mut end = start + low.max(1);
                    if end < graphemes.len() {
                        if let Some(boundary) = (start + 1..=end)
                            .rev()
                            .find(|i| opportunities.contains(&positions[*i]))
                        {
                            end = boundary;
                        }
                    }
                    let line_start = positions[start];
                    let raw_end = positions[end];
                    let line_end = text[line_start..raw_end]
                        .trim_end_matches(breakable_space)
                        .len()
                        + line_start;
                    let line_width = advance(line_start, line_end, &mut measured, &mut work)?;
                    widest = widest.max(line_width);
                    last_height = line_height(&runs, line_start, line_end, size);
                    total_height += last_height * line_spacing;
                    start = end;
                    while start < graphemes.len() && graphemes[start].1.chars().all(breakable_space)
                    {
                        start += 1;
                    }
                }
            }
            if paragraph_end == text.len() {
                break;
            }
            paragraph_start = paragraph_end + 1;
            if paragraph_start == text.len() {
                last_height = size;
                total_height += last_height * line_spacing;
                break;
            }
        }
        text.clear();
        // LineHeight adds spacing between lines, not after the final line.
        total_height -= last_height * (line_spacing - 1.0);
        if [widest, total_height]
            .iter()
            .any(|n| !n.is_finite() || !(0.0..=4_194_304.0).contains(n))
        {
            return Err("Text layout extent limit exceeded".into());
        }
        Ok(json!([widest.ceil() as u64, total_height.ceil() as u64]))
    }

    fn make_runs(
        &self,
        source: &str,
        base_size: f64,
        base_family: &str,
        base_weight: u16,
        base_italic: bool,
        layout_scale: f64,
        input: Option<&Value>,
    ) -> Result<(String, Vec<Run>), String> {
        let default = [json!({"text":source,"style":{}})];
        let values = match input {
            None => &default,
            Some(Value::Array(values)) => values.as_slice(),
            _ => return Err("Invalid text runs".into()),
        };
        if values.len() > MAX_RUNS {
            return Err("Invalid text runs".into());
        }
        let mut raw = String::new();
        let mut text = String::new();
        let mut runs: Vec<Run> = Vec::new();
        let mut families = HashMap::new();
        let mut loaded_fonts: HashMap<PathBuf, LoadedFont> = HashMap::new();
        let mut font_bytes = 0usize;
        let mut variants = std::collections::HashSet::new();
        let mut previous_cr = false;
        for value in values {
            let part = value
                .get("text")
                .and_then(Value::as_str)
                .ok_or("Invalid text run")?;
            let overrides = value
                .get("style")
                .and_then(Value::as_object)
                .ok_or("Invalid text run")?;
            if raw.len().saturating_add(part.len()) > MAX_TEXT_BYTES {
                return Err("Invalid text runs".into());
            }
            raw.push_str(part);
            let mut content = part.to_owned();
            if previous_cr && content.starts_with('\n') {
                content.remove(0);
            }
            if !part.is_empty() {
                previous_cr = part.ends_with('\r');
            }
            content = content
                .replace("\r\n", "\n")
                .replace('\r', "\n")
                .replace(
                    ['\u{0085}', '\u{2028}', '\u{2029}', '\u{000b}', '\u{000c}'],
                    "\n",
                )
                .replace('\t', " ");
            if content.is_empty() {
                continue;
            }
            let family = match overrides.get("family") {
                Some(value) => value.as_str().ok_or("Invalid displayed font descriptor")?,
                None => base_family,
            };
            let weight = match overrides.get("weight") {
                Some(v) => {
                    v.as_u64()
                        .filter(|w| (100..=900).contains(w) && w % 100 == 0)
                        .ok_or("Invalid displayed font weight or style")? as u16
                }
                None => base_weight,
            };
            let italic = match overrides.get("style") {
                Some(v) => match v.as_u64() {
                    Some(0) => false,
                    Some(1) => true,
                    _ => return Err("Invalid displayed font weight or style".into()),
                },
                None => base_italic,
            };
            if !family.starts_with("rbxasset://fonts/families/")
                || !(100..=900).contains(&weight)
                || weight % 100 != 0
            {
                return Err("Invalid displayed font descriptor".into());
            }
            let run_size = match overrides.get("size") {
                Some(v) => finite_number(Some(v)).ok_or("Invalid text run size")?,
                None => base_size,
            };
            let scale = match overrides.get("sizeScale") {
                Some(v) => finite_number(Some(v)).ok_or("Invalid text run size scale")?,
                None => 1.0,
            };
            if !(1.0..=1000.0).contains(&run_size) || !(0.5..=1.0).contains(&scale) {
                return Err("Invalid text run size".into());
            }
            let effective_size = run_size * scale * layout_scale;
            variants.insert((family.to_owned(), weight, italic, effective_size.to_bits()));
            if variants.len() > MAX_VARIANTS {
                return Err("Text measurement font variant limit exceeded".into());
            }
            if !families.contains_key(family) {
                families.insert(family.to_owned(), self.family_data(family)?);
            }
            let (family_name, faces, _) = &families[family];
            let face =
                select_font_face(&faces, weight, italic).ok_or("Font face is unavailable")?;
            let path = self.font_path(&family_name, face)?;
            let loaded = if let Some(loaded) = loaded_fonts.get(&path) {
                loaded.clone()
            } else {
                // Reject the next file before reading it if this request
                // already retains too much font data across its rich runs.
                let size = fs::metadata(&path)
                    .map_err(|_| "Font asset unavailable")?
                    .len();
                if size > MAX_REQUEST_FONT_BYTES.saturating_sub(font_bytes) as u64 {
                    return Err("Text measurement font data limit exceeded".into());
                }
                let loaded = self.load_font(&path)?;
                font_bytes += loaded.bytes;
                if font_bytes > MAX_REQUEST_FONT_BYTES {
                    return Err("Text measurement font data limit exceeded".into());
                }
                loaded_fonts.insert(path, loaded.clone());
                loaded
            };
            let start = text.len();
            text.push_str(&content);
            let end = text.len();
            if let Some(last) = runs.last_mut() {
                if last.end == start
                    && last.spec.family == family
                    && last.spec.weight == weight
                    && last.spec.italic == italic
                    && last.spec.size == effective_size
                {
                    last.end = end;
                    continue;
                }
            }
            runs.push(Run {
                start,
                end,
                spec: FontSpec {
                    family: family.to_owned(),
                    weight,
                    italic,
                    size: effective_size,
                    loaded,
                },
            });
        }
        if raw != source {
            return Err("Text runs do not match their source".into());
        }
        Ok((text, runs))
    }
}

impl TextServiceBackend for LocalTextBackend {
    fn handle(&self, request: &TextRequest) -> Result<Value, String> {
        match request {
            TextRequest::Family { family, .. } => {
                let (name, faces, _) = self.family_data(family)?;
                if faces.iter().any(|face| face.name.is_none()) {
                    return Err("Font family face name is unavailable".into());
                }
                Ok(
                    json!({"Name":name,"Faces":faces.iter().map(|face|json!({"Name":face.name.as_deref().unwrap_or_default(),"Weight":face.weight,"Style":u8::from(face.italic)})).collect::<Vec<_>>()}),
                )
            }
            TextRequest::Bounds { payload, .. } => self.bounds(payload),
        }
    }
}

fn finite_number(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(n) => n.as_f64().filter(|n| n.is_finite()),
        _ => None,
    }
}

// Non-breaking spaces still have a glyph advance. Only discard spaces which
// can actually separate lines; grapheme boundaries also protect emoji/marks.
fn breakable_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\u{2000}'..='\u{2006}' | '\u{2008}'..='\u{200b}' | '\u{205f}' | '\u{3000}')
}

fn line_height(runs: &[Run], start: usize, end: usize, base: f64) -> f64 {
    let height = runs
        .iter()
        .filter(|r| r.start < end && r.end > start)
        .map(|r| r.spec.size)
        .fold(0.0, f64::max);
    if height == 0.0 {
        base
    } else {
        height
    }
}

#[cfg(test)]
#[path = "local_text_tests.rs"]
mod tests;
