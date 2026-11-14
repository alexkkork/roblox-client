//! Local Roblox-style GUI layout and frame projection.
//!
//! This module projects only data already present in the local world snapshot.
//! It never fetches assets or estimates glyph metrics. Unsupported automatic
//! text sizing stays unavailable until the real text backend returns metrics.

use super::world::{self, WorldRef};
use serde_json::{json, Value as Json};
use std::collections::{HashMap, HashSet};

#[cfg(test)]
#[path = "gui_tests.rs"]
mod tests;

const MAX_DEPTH: usize = 128;
const MAX_GUI: usize = 600;
const MAX_PARTS: usize = 1800;
const MAX_PLAYERS: usize = 256;
const MAX_BYTES: usize = 720_000;

#[derive(Clone, Copy, Default)]
struct Box2 {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    scale: f64,
    rotation: f64,
    attached: bool,
}

fn err(s: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(s.into())
}
fn prop(ctx: &WorldRef, id: &str, key: &str, default: Json) -> Json {
    world::get_property(ctx, id, key).unwrap_or(default)
}
fn number(v: &Json, d: f64) -> f64 {
    v.as_f64().filter(|n| n.is_finite()).unwrap_or(d)
}
fn flag(v: &Json, d: bool) -> bool {
    v.as_bool().unwrap_or(d)
}
fn udim(v: &Json, extent: f64, scale: f64) -> Option<f64> {
    let a = v.get("$udim")?.as_array()?;
    if a.len() != 2 {
        return None;
    }
    let n = number(&a[0], f64::NAN) * extent + number(&a[1], f64::NAN) * scale;
    n.is_finite().then_some(n)
}
fn udim2(v: &Json) -> Option<([f64; 2], [f64; 2])> {
    let a = v.get("$udim2")?.as_array()?;
    if a.len() != 2 {
        return None;
    }
    let x = a[0].get("$udim")?.as_array()?;
    let y = a[1].get("$udim")?.as_array()?;
    if x.len() != 2 || y.len() != 2 {
        return None;
    }
    let values = [
        number(&x[0], f64::NAN),
        number(&x[1], f64::NAN),
        number(&y[0], f64::NAN),
        number(&y[1], f64::NAN),
    ];
    values
        .iter()
        .all(|v| v.is_finite())
        .then_some(([values[0], values[1]], [values[2], values[3]]))
}
fn vector2(v: &Json) -> Option<[f64; 2]> {
    let a = v.get("$vector2")?.as_array()?;
    (a.len() == 2).then(|| [number(&a[0], f64::NAN), number(&a[1], f64::NAN)])
}
fn enum_value(v: &Json) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.get("$enum")?.get("value")?.as_i64())
}
fn scrolling_axes(v: &Json) -> (bool, bool) {
    match v
        .get("$enum")
        .and_then(|e| e.get("name"))
        .and_then(Json::as_str)
    {
        Some("X") => (true, false),
        Some("Y") => (false, true),
        Some("XY") => (true, true),
        _ => match enum_value(v).unwrap_or(4) {
            1 => (true, false),
            2 => (false, true),
            4 => (true, true),
            0 => (true, false),
            _ => (true, true),
        },
    }
}
fn decode_entities(input: &str) -> String {
    input
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}
fn rich_text(source: &str) -> Option<(String, Json)> {
    if source.len() > 4096 || !source.is_char_boundary(source.len()) {
        return None;
    }
    let mut text = String::new();
    let mut runs: Vec<Json> = Vec::new();
    let mut styles: Vec<(String, Json, u8)> = vec![(String::new(), json!({}), 0)];
    let mut pos = 0;
    while pos < source.len() {
        if source.as_bytes()[pos] == b'<' {
            let end = source[pos..].find('>')? + pos;
            let raw = source[pos + 1..end].trim();
            if raw == "br/" || raw == "br /" {
                append_cased(
                    &mut text,
                    &mut runs,
                    "\n",
                    &styles.last()?.1,
                    styles.last()?.2,
                    None,
                );
                pos = end + 1;
                continue;
            }
            if let Some(tag) = raw.strip_prefix('/') {
                if styles.len() <= 1 || styles.last()?.0 != tag {
                    return None;
                }
                styles.pop();
                pos = end + 1;
                continue;
            }
            let split = raw.find(char::is_whitespace).unwrap_or(raw.len());
            let tag = &raw[..split];
            if !matches!(
                tag,
                "b" | "i"
                    | "u"
                    | "s"
                    | "font"
                    | "stroke"
                    | "mark"
                    | "uppercase"
                    | "uc"
                    | "smallcaps"
                    | "sc"
            ) {
                return None;
            }
            let mut style = styles.last()?.1.clone();
            let mut casing = styles.last()?.2;
            match tag {
                "b" => {
                    style["weight"] = json!(style
                        .get("weight")
                        .and_then(Json::as_u64)
                        .unwrap_or(400)
                        .max(700));
                }
                "i" => style["style"] = json!(1),
                "u" => style["underline"] = json!(true),
                "s" => style["strike"] = json!(true),
                "mark" => {
                    style["mark"] = json!([255, 255, 0]);
                    style["markTransparency"] = json!(0)
                }
                "stroke" => {
                    style["stroke"] = json!({"color":[0,0,0],"thickness":1,"transparency":0,"joins":"round","sizing":"fixed"})
                }
                "uppercase" | "uc" => casing = 1,
                "smallcaps" | "sc" => casing = 2,
                _ => {}
            }
            if matches!(tag, "font" | "stroke" | "mark")
                && !parse_rich_attributes(&raw[split..], tag, &mut style)
            {
                return None;
            }
            styles.push((tag.into(), style, casing));
            if styles.len() > 33 {
                return None;
            }
            pos = end + 1;
            continue;
        }
        let end = source[pos..]
            .find('<')
            .map(|n| pos + n)
            .unwrap_or(source.len());
        if end <= pos {
            return None;
        }
        let decoded = decode_entities(&source[pos..end]);
        append_cased(
            &mut text,
            &mut runs,
            &decoded,
            &styles.last()?.1,
            styles.last()?.2,
            None,
        );
        pos = end;
    }
    if styles.len() != 1 || text.len() > 4096 || runs.len() > 128 {
        return None;
    }
    let runs = Json::Array(runs);
    if runs.to_string().len() > 12 * 1024 {
        return None;
    }
    Some((text, runs))
}

pub(crate) fn parse_rich_text(source: &str) -> Option<(String, Json)> {
    rich_text(source)
}

fn append_run(runs: &mut Vec<Json>, text: &str, style: Json) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = runs.last_mut() {
        if last.get("style") == Some(&style) {
            if let Some(t) = last
                .get_mut("text")
                .and_then(|value| value.as_str().map(ToOwned::to_owned))
            {
                last["text"] = json!(format!("{t}{text}"));
                return;
            }
        }
    }
    runs.push(json!({"text":text,"style":style}));
}
fn append_cased(
    plain: &mut String,
    runs: &mut Vec<Json>,
    source: &str,
    style: &Json,
    casing: u8,
    previous: Option<bool>,
) {
    if casing == 0 {
        plain.push_str(source);
        append_run(runs, source, style.clone());
        return;
    }
    let mut prev = previous.unwrap_or(false);
    let mut segment = String::new();
    let mut segment_small = prev;
    let mut flush = |segment: &mut String, small: bool| {
        if !segment.is_empty() {
            let mut selected = style.clone();
            if small {
                selected["sizeScale"] = json!(0.8)
            }
            plain.push_str(segment);
            append_run(runs, segment, selected);
            segment.clear();
        }
    };
    for ch in source.chars() {
        let upper: String = ch.to_uppercase().collect();
        let combining = matches!(ch as u32,0x0300..=0x036f|0x1ab0..=0x1aff|0x1dc0..=0x1dff|0x20d0..=0x20ff|0xfe20..=0xfe2f);
        let small = casing == 2
            && if combining {
                prev
            } else {
                upper != ch.to_string()
            };
        if casing == 2 && small != segment_small {
            flush(&mut segment, segment_small);
            segment_small = small;
        }
        if casing == 2 {
            prev = small
        }
        segment.push_str(&upper);
    }
    flush(&mut segment, segment_small);
}
fn parse_rich_attributes(input: &str, tag: &str, style: &mut Json) -> bool {
    let mut rest = input.trim();
    let mut seen = HashSet::new();
    while !rest.is_empty() {
        let Some(eq) = rest.find('=') else {
            return false;
        };
        let key = rest[..eq].trim();
        if key.is_empty() || !seen.insert(key) {
            return false;
        }
        rest = rest[eq + 1..].trim_start();
        let Some(quote) = rest.chars().next().filter(|ch| *ch == '"' || *ch == '\'') else {
            return false;
        };
        let Some(tail) = rest.strip_prefix(quote) else {
            return false;
        };
        let Some(end) = tail.find(quote) else {
            return false;
        };
        let value = &tail[..end];
        rest = tail[end + 1..].trim_start();
        let key = match (tag, key) {
            ("stroke", "th") => "thickness",
            ("stroke", "tr") => "transparency",
            _ => key,
        };
        match (tag, key) {
            ("font", "color") | ("mark", "color") | ("stroke", "color") => {
                let Some(rgb) = parse_hex_color(value) else {
                    return false;
                };
                if tag == "stroke" {
                    style["stroke"]["color"] = json!(rgb)
                } else if tag == "mark" {
                    style["mark"] = json!(rgb)
                } else {
                    style["color"] = json!(rgb)
                }
            }
            ("font", "size") => {
                let Ok(n) = value.parse::<f64>() else {
                    return false;
                };
                if !(1.0..=1000.).contains(&n) {
                    return false;
                }
                style["size"] = json!(n)
            }
            ("font", "weight") => {
                let parsed = value
                    .parse::<u16>()
                    .ok()
                    .filter(|n| (100..=900).contains(n) && n % 100 == 0);
                let named = match value.to_ascii_lowercase().as_str() {
                    "thin" => Some(100),
                    "extralight" => Some(200),
                    "light" => Some(300),
                    "regular" => Some(400),
                    "medium" => Some(500),
                    "semibold" => Some(600),
                    "bold" => Some(700),
                    "extrabold" => Some(800),
                    "heavy" => Some(900),
                    _ => None,
                };
                let Some(n) = parsed.or(named) else {
                    return false;
                };
                style["weight"] = json!(n)
            }
            ("font", "family") => {
                if value.len() > 512
                    || !(value.starts_with("rbxasset://fonts/families/")
                        || value.starts_with("rbxassetid://"))
                {
                    return false;
                }
                style["family"] = json!(value)
            }
            ("font" | "mark" | "stroke", "transparency") => {
                let Ok(n) = value.parse::<f64>() else {
                    return false;
                };
                if !(0.0..=1.).contains(&n) {
                    return false;
                }
                if tag == "stroke" {
                    style["stroke"]["transparency"] = json!(n)
                } else if tag == "mark" {
                    style["markTransparency"] = json!(n)
                } else {
                    style["transparency"] = json!(n)
                }
            }
            ("stroke", "thickness") => {
                let Ok(n) = value.parse::<f64>() else {
                    return false;
                };
                if !(0.0..=1000.).contains(&n) {
                    return false;
                }
                style["stroke"]["thickness"] = json!(n)
            }
            ("stroke", "joins") if matches!(value, "round" | "bevel" | "miter") => {
                style["stroke"]["joins"] = json!(value)
            }
            ("stroke", "sizing") if matches!(value, "fixed" | "scaled") => {
                style["stroke"]["sizing"] = json!(value)
            }
            _ => return false,
        }
    }
    true
}
fn parse_hex_color(s: &str) -> Option<[u8; 3]> {
    if s.len() != 7 || !s.starts_with('#') {
        return None;
    }
    Some([
        u8::from_str_radix(&s[1..3], 16).ok()?,
        u8::from_str_radix(&s[3..5], 16).ok()?,
        u8::from_str_radix(&s[5..7], 16).ok()?,
    ])
}
fn increment(frame: &mut Json, key: &str) {
    let value = frame.get(key).and_then(Json::as_u64).unwrap_or(0);
    frame[key] = json!(value + 1);
}
fn parent(ctx: &WorldRef, id: &str) -> Option<String> {
    ctx.borrow().nodes.get(id).and_then(|n| n.parent.clone())
}
fn local_player(ctx: &WorldRef) -> Option<String> {
    ctx.borrow()
        .snapshot
        .get("local_player")
        .and_then(Json::as_str)
        .map(ToOwned::to_owned)
}
fn class(ctx: &WorldRef, id: &str) -> String {
    ctx.borrow()
        .nodes
        .get(id)
        .map(|n| n.class_name.clone())
        .unwrap_or_default()
}
fn component(ctx: &WorldRef, id: &str, wanted: &str) -> Option<String> {
    world::children(ctx, id)
        .into_iter()
        .find(|child| world::is_a(ctx, child, wanted))
}
fn read_udim2(ctx: &WorldRef, id: &str, key: &str) -> Option<([f64; 2], [f64; 2])> {
    udim2(&prop(ctx, id, key, Json::Null))
}
fn text_measure(ctx: &WorldRef, id: &str, width: f64) -> Result<[f64; 2], String> {
    let text = prop(ctx, id, "Text", Json::Null)
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or("text content unavailable")?;
    let rich = flag(&prop(ctx, id, "RichText", json!(false)), false);
    let (text, runs) = if rich {
        rich_text(&text).ok_or("rich text is malformed or unsupported")?
    } else {
        (text, Json::Null)
    };
    let font = prop(ctx, id, "FontFace", Json::Null)
        .get("$font")
        .cloned()
        .ok_or("text font unavailable")?;
    let mut request = json!({"request_id":1,"kind":"bounds","text":text,"font":font,
        "size":number(&prop(ctx,id,"TextSize",json!(14)),14.),"width":width,
        "line_height":number(&prop(ctx,id,"LineHeight",json!(1)),1.)});
    if rich {
        request["runs"] = runs;
    }
    let raw = serde_json::to_vec(&request).map_err(|_| "invalid text measure request")?;
    crate::engine_text_measure::measure(&raw)
}

fn box_for(
    ctx: &WorldRef,
    id: &str,
    display: &Json,
    cache: &mut HashMap<String, Box2>,
    visiting: &mut HashSet<String>,
    depth: usize,
) -> Result<Box2, String> {
    if let Some(b) = cache.get(id) {
        return Ok(*b);
    }
    if depth > MAX_DEPTH || !visiting.insert(id.into()) {
        return Err("GUI ancestry cycle or depth limit".into());
    }
    let cls = class(ctx, id);
    let mut b = Box2::default();
    if cls == "ScreenGui" {
        let p = parent(ctx, id);
        let attached = p.as_ref().is_some_and(|p| {
            class(ctx, p) == "CoreGui"
                || (class(ctx, p) == "PlayerGui" && parent(ctx, p) == local_player(ctx))
        });
        let size = display
            .get("size")
            .and_then(Json::as_array)
            .filter(|a| a.len() == 2);
        if attached {
            if let Some(size) = size {
                let w = number(&size[0], 0.);
                let h = number(&size[1], 0.);
                let explicit_mode = prop(ctx, id, "ScreenInsets", Json::Null);
                let mode = enum_value(&explicit_mode).unwrap_or_else(|| {
                    if flag(&prop(ctx, id, "IgnoreGuiInset", Json::Null), false) {
                        1
                    } else {
                        2
                    }
                });
                let inset_key = match mode {
                    0 => None,
                    1 => Some("device_insets"),
                    2 => Some("core_insets"),
                    3 => Some("topbar_insets"),
                    _ => return Err("unknown ScreenInsets value".into()),
                };
                if mode == 3 && display.get("topbar_insets").is_none() {
                    return Err("TopbarSafeInsets requires host topbar area".into());
                }
                let margins = inset_key
                    .and_then(|key| display.get(key))
                    .and_then(Json::as_array)
                    .filter(|a| a.len() == 4);
                let m = margins
                    .map(|a| {
                        [
                            number(&a[0], 0.),
                            number(&a[1], 0.),
                            number(&a[2], 0.),
                            number(&a[3], 0.),
                        ]
                    })
                    .unwrap_or([0.; 4]);
                let core = display
                    .get("core_insets")
                    .and_then(Json::as_array)
                    .filter(|a| a.len() == 4);
                let c = core
                    .map(|a| {
                        [
                            number(&a[0], 0.),
                            number(&a[1], 0.),
                            number(&a[2], 0.),
                            number(&a[3], 0.),
                        ]
                    })
                    .unwrap_or([0.; 4]);
                b = Box2 {
                    x: m[0] - c[0],
                    y: m[1] - c[1],
                    w: (w - m[0] - m[2]).max(0.),
                    h: (h - m[1] - m[3]).max(0.),
                    scale: 1.,
                    attached: w > 0. && h > 0.,
                    ..Box2::default()
                };
            }
        }
    } else if world::is_a(ctx, id, "GuiObject") {
        let p = parent(ctx, id).ok_or("GUI object has no parent")?;
        let mut pb = box_for(ctx, &p, display, cache, visiting, depth + 1)?;
        if !pb.attached {
            visiting.remove(id);
            cache.insert(id.into(), b);
            return Ok(b);
        }
        if world::is_a(ctx, &p, "GuiObject") {
            if component(ctx, &p, "UIGridLayout").is_some()
                && component(ctx, &p, "UIListLayout").is_some()
            {
                return Err("multiple content layouts are unsupported".into());
            }
            for child in world::children(ctx, id) {
                let child_class = class(ctx, &child);
                if world::is_a(ctx, &child, "UIConstraint")
                    && !matches!(
                        child_class.as_str(),
                        "UISizeConstraint" | "UIAspectRatioConstraint" | "UITextSizeConstraint"
                    )
                {
                    return Err(format!("unsupported GUI size constraint {child_class}"));
                }
            }
            if class(ctx, &p) == "ScrollingFrame"
                && flag(&prop(ctx, &p, "ScrollingEnabled", json!(true)), true)
            {
                let scale = pb.scale;
                let extent = scroll_canvas_extent(ctx, &p, pb);
                if let Some(offset) =
                    vector2(&prop(ctx, &p, "CanvasPosition", json!({"$vector2":[0,0]})))
                {
                    pb.x -= (offset[0] * scale).clamp(0., (extent[0] - pb.w).max(0.));
                    pb.y -= (offset[1] * scale).clamp(0., (extent[1] - pb.h).max(0.));
                }
            }
            if let Some(layout) = component(ctx, &p, "UIPadding") {
                let l = udim(
                    &prop(ctx, &layout, "PaddingLeft", json!({"$udim":[0,0]})),
                    pb.w,
                    pb.scale,
                )
                .unwrap_or(0.);
                let r = udim(
                    &prop(ctx, &layout, "PaddingRight", json!({"$udim":[0,0]})),
                    pb.w,
                    pb.scale,
                )
                .unwrap_or(0.);
                let t = udim(
                    &prop(ctx, &layout, "PaddingTop", json!({"$udim":[0,0]})),
                    pb.h,
                    pb.scale,
                )
                .unwrap_or(0.);
                let bot = udim(
                    &prop(ctx, &layout, "PaddingBottom", json!({"$udim":[0,0]})),
                    pb.h,
                    pb.scale,
                )
                .unwrap_or(0.);
                pb.x += l;
                pb.y += t;
                pb.w = (pb.w - l - r).max(0.);
                pb.h = (pb.h - t - bot).max(0.);
            }
        }
        if pb.rotation != 0. {
            return Err("rotated GUI ancestors are unsupported".into());
        }
        let (sx, sy) =
            read_udim2(ctx, id, "Size").ok_or_else(|| format!("GUI Size unavailable for {cls}"))?;
        let (px, py) = read_udim2(ctx, id, "Position")
            .ok_or_else(|| format!("GUI Position unavailable for {cls}"))?;
        let anchor =
            vector2(&prop(ctx, id, "AnchorPoint", json!({"$vector2":[0,0]}))).unwrap_or([0.; 2]);
        let own_scale = component(ctx, id, "UIScale")
            .map(|x| number(&prop(ctx, &x, "Scale", json!(1)), 1.))
            .unwrap_or(1.);
        if own_scale < 0. || !own_scale.is_finite() {
            return Err("invalid UIScale".into());
        }
        let constraint = enum_value(&prop(
            ctx,
            id,
            "SizeConstraint",
            json!({"$enum":{"value":0}}),
        ))
        .unwrap_or(0);
        b.w = (sx[0] * (if constraint == 2 { pb.h } else { pb.w }) + sx[1] * pb.scale).max(0.)
            * own_scale;
        b.h = (sy[0] * (if constraint == 1 { pb.w } else { pb.h }) + sy[1] * pb.scale).max(0.)
            * own_scale;
        b.scale = pb.scale * own_scale;
        b.attached = true;
        if component(ctx, id, "UISizeConstraint").is_some()
            && component(ctx, id, "UIAspectRatioConstraint").is_some()
        {
            return Err("combined size and aspect constraints are unsupported".into());
        }
        if let Some(bounds) = component(ctx, id, "UISizeConstraint") {
            let min = vector2(&prop(ctx, &bounds, "MinSize", json!({"$vector2":[0,0]})))
                .unwrap_or([0.; 2]);
            let max = vector2(&prop(
                ctx,
                &bounds,
                "MaxSize",
                json!({"$vector2":[{"$number":"inf"},{"$number":"inf"}]}),
            ))
            .unwrap_or([f64::INFINITY; 2]);
            b.w = b.w.max(min[0] * b.scale).min(max[0] * b.scale);
            b.h = b.h.max(min[1] * b.scale).min(max[1] * b.scale);
        }
        let automatic = enum_value(&prop(
            ctx,
            id,
            "AutomaticSize",
            json!({"$enum":{"value":0}}),
        ))
        .unwrap_or(0);
        let in_grid = component(ctx, &p, "UIGridLayout").is_some();
        if automatic != 0 && !in_grid {
            if cls == "ScrollingFrame" {
                return Err("ScrollingFrame automatic sizing uses AutomaticCanvasSize".into());
            }
            b = automatic_size(
                ctx,
                id,
                display,
                pb,
                (px, py),
                anchor,
                own_scale,
                automatic,
                b,
            )?;
        }
        if let Some(aspect) = component(ctx, id, "UIAspectRatioConstraint") {
            let ratio = number(&prop(ctx, &aspect, "AspectRatio", json!(1)), 1.);
            if ratio <= 0. {
                return Err("AspectRatio must be greater than zero".into());
            }
            if enum_value(&prop(
                ctx,
                &aspect,
                "AspectType",
                json!({"$enum":{"value":0}}),
            ))
            .unwrap_or(0)
                == 1
            {
                if enum_value(&prop(
                    ctx,
                    &aspect,
                    "DominantAxis",
                    json!({"$enum":{"value":0}}),
                ))
                .unwrap_or(0)
                    == 0
                {
                    b.w = b.w.min(pb.w * own_scale);
                    b.h = pb.h * own_scale;
                } else {
                    b.w = pb.w * own_scale;
                    b.h = b.h.min(pb.h * own_scale)
                }
            }
            if b.w == 0. || b.h == 0. {
                b.w = 0.;
                b.h = 0.
            } else if b.w / b.h > ratio {
                b.w = b.h * ratio
            } else {
                b.h = b.w / ratio
            }
        }
        b.x = pb.x + px[0] * pb.w + px[1] * pb.scale - anchor[0] * b.w;
        b.y = pb.y + py[0] * pb.h + py[1] * pb.scale - anchor[1] * b.h;
        b.rotation = number(&prop(ctx, id, "Rotation", json!(0)), 0.);
        if component(ctx, &p, "UIGridLayout").is_some() {
            apply_grid_layout(ctx, &p, id, pb, &mut b)?
        } else {
            apply_list_layout(ctx, &p, id, pb, &mut b)?
        }
    }
    visiting.remove(id);
    cache.insert(id.into(), b);
    Ok(b)
}

fn automatic_size(
    ctx: &WorldRef,
    id: &str,
    display: &Json,
    parent: Box2,
    position: ([f64; 2], [f64; 2]),
    anchor: [f64; 2],
    own_scale: f64,
    automatic: i64,
    base: Box2,
) -> Result<Box2, String> {
    let cls = class(ctx, id);
    let text_node = matches!(cls.as_str(), "TextLabel" | "TextButton" | "TextBox");
    if text_node && flag(&prop(ctx, id, "TextScaled", json!(false)), false) {
        return Err("TextScaled with AutomaticSize is unsupported".into());
    }
    let mut candidate = base;
    let mut work = 0usize;
    for _ in 0..64 {
        candidate.x = parent.x + position.0[0] * parent.w + position.0[1] * parent.scale
            - anchor[0] * candidate.w;
        candidate.y = parent.y + position.1[0] * parent.h + position.1[1] * parent.scale
            - anchor[1] * candidate.h;
        let mut local = HashMap::new();
        local.insert(id.to_owned(), candidate);
        let mut area = candidate;
        let mut padding = [0.; 4];
        if let Some(component) = component(ctx, id, "UIPadding") {
            padding[0] = udim(
                &prop(ctx, &component, "PaddingLeft", json!({"$udim":[0,0]})),
                area.w,
                area.scale,
            )
            .unwrap_or(0.);
            padding[1] = udim(
                &prop(ctx, &component, "PaddingRight", json!({"$udim":[0,0]})),
                area.w,
                area.scale,
            )
            .unwrap_or(0.);
            padding[2] = udim(
                &prop(ctx, &component, "PaddingTop", json!({"$udim":[0,0]})),
                area.h,
                area.scale,
            )
            .unwrap_or(0.);
            padding[3] = udim(
                &prop(ctx, &component, "PaddingBottom", json!({"$udim":[0,0]})),
                area.h,
                area.scale,
            )
            .unwrap_or(0.);
            area.x += padding[0];
            area.y += padding[2];
            area.w = (area.w - padding[0] - padding[1]).max(0.);
            area.h = (area.h - padding[2] - padding[3]).max(0.);
        }
        let mut extent = [0.0f64; 2];
        let children = world::children(ctx, id);
        work = work.saturating_add(children.len().max(1));
        if work > 1_000_000 {
            return Err("automatic GUI measurement work limit exceeded".into());
        }
        for child in children {
            if !world::is_a(ctx, &child, "GuiObject")
                || !flag(&prop(ctx, &child, "Visible", json!(true)), true)
            {
                continue;
            }
            let b = box_for(ctx, &child, display, &mut local, &mut HashSet::new(), 0)?;
            if b.rotation != 0. {
                return Err("automatic sizing with rotated content is unsupported".into());
            }
            extent[0] = extent[0].max(b.x - area.x + b.w);
            extent[1] = extent[1].max(b.y - area.y + b.h);
        }
        let mut next = base;
        if automatic & 1 != 0 {
            next.w = next.w.max(extent[0] + padding[0] + padding[1]);
        }
        if automatic & 2 != 0 {
            next.h = next.h.max(extent[1] + padding[2] + padding[3]);
        }
        if text_node {
            if automatic & 1 != 0 {
                let measured = text_measure(ctx, id, 0.)?;
                next.w = next
                    .w
                    .max(measured[0] * base.scale + padding[0] + padding[1]);
            }
            if automatic & 2 != 0 {
                let width = if flag(&prop(ctx, id, "TextWrapped", json!(false)), false) {
                    (next.w - padding[0] - padding[1]).max(0.) / base.scale.max(f64::MIN_POSITIVE)
                } else {
                    0.
                };
                let measured = text_measure(ctx, id, width)?;
                next.h = next
                    .h
                    .max(measured[1] * base.scale + padding[2] + padding[3]);
            }
        }
        if let Some(bounds) = component(ctx, id, "UISizeConstraint") {
            let min = vector2(&prop(ctx, &bounds, "MinSize", json!({"$vector2":[0,0]})))
                .unwrap_or([0.; 2]);
            let max = vector2(&prop(
                ctx,
                &bounds,
                "MaxSize",
                json!({"$vector2":[{"$number":"inf"},{"$number":"inf"}]}),
            ))
            .unwrap_or([f64::INFINITY; 2]);
            next.w = next.w.max(min[0] * base.scale).min(max[0] * base.scale);
            next.h = next.h.max(min[1] * base.scale).min(max[1] * base.scale);
        }
        if (next.w - candidate.w).abs() <= 1e-5 && (next.h - candidate.h).abs() <= 1e-5 {
            next.x = parent.x + position.0[0] * parent.w + position.0[1] * parent.scale
                - anchor[0] * next.w;
            next.y = parent.y + position.1[0] * parent.h + position.1[1] * parent.scale
                - anchor[1] * next.h;
            return Ok(next);
        }
        candidate = next;
    }
    let _ = own_scale;
    Err("automatic GUI sizing did not converge".into())
}

fn apply_list_layout(
    ctx: &WorldRef,
    parent_id: &str,
    id: &str,
    area: Box2,
    b: &mut Box2,
) -> Result<(), String> {
    let Some(layout) = world::children(ctx, parent_id)
        .into_iter()
        .find(|child| class(ctx, child) == "UIListLayout")
    else {
        return Ok(());
    };
    let direction = enum_value(&prop(
        ctx,
        &layout,
        "FillDirection",
        json!({"$enum":{"value":1}}),
    ))
    .unwrap_or(1);
    if direction != 0 && direction != 1 {
        return Err("invalid UIListLayout.FillDirection".into());
    }
    let sort = enum_value(&prop(
        ctx,
        &layout,
        "SortOrder",
        json!({"$enum":{"value":0}}),
    ))
    .unwrap_or(0);
    if sort == 1 {
        return Err("UIListLayout custom sorting is unavailable".into());
    }
    let wraps = flag(&prop(ctx, &layout, "Wraps", json!(false)), false);
    let horizontal = enum_value(&prop(
        ctx,
        &layout,
        "HorizontalAlignment",
        json!({"$enum":{"value":1}}),
    ))
    .unwrap_or(1);
    let vertical = enum_value(&prop(
        ctx,
        &layout,
        "VerticalAlignment",
        json!({"$enum":{"value":1}}),
    ))
    .unwrap_or(1);
    let pad = udim(
        &prop(ctx, &layout, "Padding", json!({"$udim":[0,0]})),
        if direction == 0 { area.w } else { area.h },
        area.scale,
    )
    .unwrap_or(0.);
    let main_flex = enum_value(&prop(
        ctx,
        &layout,
        if direction == 0 {
            "HorizontalFlex"
        } else {
            "VerticalFlex"
        },
        json!({"$enum":{"value":0}}),
    ))
    .unwrap_or(0);
    if !(0..=4).contains(&main_flex) {
        return Err("invalid UIListLayout flex mode".into());
    }
    let cross_flex_mode = enum_value(&prop(
        ctx,
        &layout,
        if direction == 0 {
            "VerticalFlex"
        } else {
            "HorizontalFlex"
        },
        json!({"$enum":{"value":0}}),
    ))
    .unwrap_or(0);
    let line_alignment = enum_value(&prop(
        ctx,
        &layout,
        "ItemLineAlignment",
        json!({"$enum":{"value":0}}),
    ))
    .unwrap_or(0);
    struct Item {
        id: String,
        w: f64,
        h: f64,
        name: String,
        order: f64,
        anchor: [f64; 2],
        min: [f64; 2],
        max: [f64; 2],
        grow: f64,
        shrink: f64,
        alignment: i64,
    }
    let mut items = Vec::new();
    for child in world::children(ctx, parent_id) {
        if !world::is_a(ctx, &child, "GuiObject")
            || !flag(&prop(ctx, &child, "Visible", json!(true)), true)
        {
            continue;
        }
        let (sx, sy) = read_udim2(ctx, &child, "Size")
            .ok_or_else(|| format!("UI list child {child} has no Size"))?;
        let scale = area.scale
            * component(ctx, &child, "UIScale")
                .map(|id| number(&prop(ctx, &id, "Scale", json!(1)), 1.))
                .unwrap_or(1.);
        let constraint = enum_value(&prop(
            ctx,
            &child,
            "SizeConstraint",
            json!({"$enum":{"value":0}}),
        ))
        .unwrap_or(0);
        let mut w = (sx[0] * (if constraint == 2 { area.h } else { area.w }) + sx[1] * area.scale)
            .max(0.)
            * scale
            / area.scale.max(f64::MIN_POSITIVE);
        let mut h = (sy[0] * (if constraint == 1 { area.w } else { area.h }) + sy[1] * area.scale)
            .max(0.)
            * scale
            / area.scale.max(f64::MIN_POSITIVE);
        let mut min = [0., 0.];
        let mut max = [f64::INFINITY; 2];
        if let Some(bounds) = component(ctx, &child, "UISizeConstraint") {
            let bound_min = vector2(&prop(ctx, &bounds, "MinSize", json!({"$vector2":[0,0]})))
                .unwrap_or([0.; 2]);
            let bound_max = vector2(&prop(
                ctx,
                &bounds,
                "MaxSize",
                json!({"$vector2":[{"$number":"inf"},{"$number":"inf"}]}),
            ))
            .unwrap_or([f64::INFINITY; 2]);
            min = [bound_min[0] * scale, bound_min[1] * scale];
            max = [bound_max[0] * scale, bound_max[1] * scale];
            w = w.max(min[0]).min(max[0]);
            h = h.max(min[1]).min(max[1]);
        }
        let mut grow = if main_flex == 1 { 1. } else { 0. };
        let mut shrink = 0.;
        let mut alignment = line_alignment;
        if alignment == 0 && cross_flex_mode != 0 {
            alignment = 4
        }
        if let Some(flex) = component(ctx, &child, "UIFlexItem") {
            grow = 0.;
            shrink = 0.;
            match enum_value(&prop(ctx, &flex, "FlexMode", json!({"$enum":{"value":0}})))
                .unwrap_or(0)
            {
                0 => {}
                1 => grow = 1.,
                2 => shrink = 1.,
                3 => {
                    grow = 1.;
                    shrink = 1.
                }
                4 => {
                    grow = number(&prop(ctx, &flex, "GrowRatio", json!(0)), 0.);
                    shrink = number(&prop(ctx, &flex, "ShrinkRatio", json!(0)), 0.)
                }
                _ => return Err("invalid UIFlexItem.FlexMode".into()),
            }
            let own_alignment = enum_value(&prop(
                ctx,
                &flex,
                "ItemLineAlignment",
                json!({"$enum":{"value":0}}),
            ))
            .unwrap_or(0);
            if own_alignment != 0 {
                alignment = own_alignment;
            }
        }
        if grow < 0. || shrink < 0. || !grow.is_finite() || !shrink.is_finite() {
            return Err("UIFlexItem ratios must be finite and nonnegative".into());
        }
        if component(ctx, &child, "UIAspectRatioConstraint").is_some() {
            min = [w, h];
            max = [w, h];
        }
        let anchor = vector2(&prop(ctx, &child, "AnchorPoint", json!({"$vector2":[0,0]})))
            .unwrap_or([0.; 2]);
        let name = ctx
            .borrow()
            .nodes
            .get(&child)
            .map(|n| n.name.clone())
            .unwrap_or_default();
        let order = number(&prop(ctx, &child, "LayoutOrder", json!(0)), 0.);
        items.push(Item {
            id: child,
            w,
            h,
            name,
            order,
            anchor,
            min,
            max,
            grow,
            shrink,
            alignment,
        });
    }
    items.sort_by(|a, b| match sort {
        0 => a.name.cmp(&b.name),
        2 => a
            .order
            .partial_cmp(&b.order)
            .unwrap_or(std::cmp::Ordering::Equal),
        _ => std::cmp::Ordering::Equal,
    });
    let main = if direction == 0 { area.w } else { area.h };
    let cross = if direction == 0 { area.h } else { area.w };
    let mut lines: Vec<Vec<usize>> = Vec::new();
    let mut line = Vec::new();
    let mut used = 0.;
    for i in 0..items.len() {
        let size = if direction == 0 {
            items[i].w
        } else {
            items[i].h
        };
        let next = used + if line.is_empty() { 0. } else { pad } + size;
        if wraps && !line.is_empty() && next > main {
            lines.push(std::mem::take(&mut line));
            used = 0.;
        }
        used += if line.is_empty() { 0. } else { pad } + size;
        line.push(i);
    }
    if !line.is_empty() {
        lines.push(line)
    }
    for line in &lines {
        let axis = if direction == 0 { 0 } else { 1 };
        let mut total = line
            .iter()
            .map(|i| if axis == 0 { items[*i].w } else { items[*i].h })
            .sum::<f64>();
        let minimum_edges = if main_flex == 2 {
            pad
        } else if main_flex == 4 {
            2. * pad
        } else {
            0.
        };
        let mut delta = main - total - pad * (line.len().saturating_sub(1) as f64) - minimum_edges;
        for _ in 0..line.len() {
            if delta.abs() < 1e-9 {
                break;
            }
            let growing = delta > 0.;
            let weight = line
                .iter()
                .map(|i| {
                    let item = &items[*i];
                    let current = if axis == 0 { item.w } else { item.h };
                    let (lo, hi) = if axis == 0 {
                        (item.min[0], item.max[0])
                    } else {
                        (item.min[1], item.max[1])
                    };
                    if (growing && current < hi) || (!growing && current > lo) {
                        if growing {
                            item.grow
                        } else {
                            item.shrink
                        }
                    } else {
                        0.
                    }
                })
                .sum::<f64>();
            if weight <= 0. {
                break;
            }
            let mut consumed = 0.;
            for i in line {
                let item = &mut items[*i];
                let current = if axis == 0 { item.w } else { item.h };
                let rate = if growing { item.grow } else { item.shrink };
                let (lo, hi) = if axis == 0 {
                    (item.min[0], item.max[0])
                } else {
                    (item.min[1], item.max[1])
                };
                if rate <= 0. || (growing && current >= hi) || (!growing && current <= lo) {
                    continue;
                }
                let next = (current + delta * rate / weight).clamp(lo, hi);
                consumed += next - current;
                if axis == 0 {
                    item.w = next
                } else {
                    item.h = next
                }
            }
            total += consumed;
            delta -= consumed;
            if consumed.abs() < 1e-9 {
                break;
            }
        }
    }
    let line_cross: Vec<f64> = lines
        .iter()
        .map(|line| {
            line.iter()
                .map(|i| {
                    if direction == 0 {
                        items[*i].h
                    } else {
                        items[*i].w
                    }
                })
                .fold(0., f64::max)
        })
        .collect();
    let cross_available = if direction == 0 { area.h } else { area.w };
    let cross_flex = cross_flex_mode;
    let mut line_cross = line_cross;
    if cross_flex == 1 && !line_cross.is_empty() {
        let count = line_cross.len();
        let free = cross_available - line_cross.iter().sum::<f64>() - pad * (count - 1) as f64;
        if free > 0. {
            let share = free / count as f64;
            for size in &mut line_cross {
                *size += share;
            }
        }
    }
    let cross_align = if direction == 0 { vertical } else { horizontal };
    let (cross_gap, cross_edge) = match cross_flex {
        2 => (
            pad.max(
                (cross_available - line_cross.iter().sum::<f64>()) / line_cross.len().max(1) as f64,
            ),
            pad.max(
                (cross_available - line_cross.iter().sum::<f64>()) / line_cross.len().max(1) as f64,
            ) * 0.5,
        ),
        3 if line_cross.len() > 1 => (
            pad.max(
                (cross_available - line_cross.iter().sum::<f64>()) / (line_cross.len() - 1) as f64,
            ),
            0.,
        ),
        4 => (
            pad.max(
                (cross_available - line_cross.iter().sum::<f64>()) / (line_cross.len() + 1) as f64,
            ),
            pad.max(
                (cross_available - line_cross.iter().sum::<f64>()) / (line_cross.len() + 1) as f64,
            ),
        ),
        _ => (pad, 0.),
    };
    let total_cross = line_cross.iter().sum::<f64>()
        + cross_gap * line_cross.len().saturating_sub(1) as f64
        + cross_edge * 2.;
    let group_cross = match cross_align {
        0 => (cross - total_cross) * 0.5,
        1 => 0.,
        _ => cross - total_cross,
    } + cross_edge;
    let mut cross_offset = group_cross;
    for (line_idx, line) in lines.iter().enumerate() {
        let item_sum = line
            .iter()
            .map(|i| {
                if direction == 0 {
                    items[*i].w
                } else {
                    items[*i].h
                }
            })
            .sum::<f64>();
        let (flow_gap, edge) = match main_flex {
            2 => (
                pad.max((main - item_sum) / line.len().max(1) as f64),
                pad.max((main - item_sum) / line.len().max(1) as f64) * 0.5,
            ),
            3 if line.len() > 1 => (pad.max((main - item_sum) / (line.len() - 1) as f64), 0.),
            4 => (
                pad.max((main - item_sum) / (line.len() + 1) as f64),
                pad.max((main - item_sum) / (line.len() + 1) as f64),
            ),
            _ => (pad, 0.),
        };
        let sum = item_sum + flow_gap * line.len().saturating_sub(1) as f64 + edge * 2.;
        let main_align = if direction == 0 { horizontal } else { vertical };
        let main_offset = if main_flex >= 2 && !(main_flex == 3 && line.len() == 1) {
            edge
        } else {
            match main_align {
                0 => (main - sum) * 0.5,
                1 => 0.,
                _ => main - sum,
            }
        };
        let mut cursor = main_offset;
        for i in line {
            let item = &items[*i];
            let mut item_w = item.w;
            let mut item_h = item.h;
            if item.alignment == 4 {
                let axis = if direction == 0 { 1 } else { 0 };
                let requested = line_cross[line_idx];
                let stretched = requested.clamp(item.min[axis], item.max[axis]);
                if direction == 0 {
                    item_h = stretched
                } else {
                    item_w = stretched
                }
            }
            let size = if direction == 0 { item_w } else { item_h };
            let child_cross = if direction == 0 { item_h } else { item_w };
            let free = line_cross[line_idx] - child_cross;
            let cross_align = if direction == 0 { vertical } else { horizontal };
            let cross_item = match item.alignment {
                0 => match cross_align {
                    0 => free * 0.5,
                    1 => 0.,
                    _ => free,
                },
                2 => free * 0.5,
                3 => free,
                4 => 0.,
                _ => 0.,
            };
            if item.id == id {
                // Flex distribution changes the item's size on either axis.
                // Preserve both results before applying the axis-specific
                // offset; otherwise main-axis growth is silently discarded.
                b.w = item_w;
                b.h = item_h;
                if direction == 0 {
                    b.x = area.x + cursor - item.anchor[0] * b.w;
                    b.y = area.y + cross_offset + cross_item - item.anchor[1] * b.h;
                } else {
                    b.y = area.y + cursor - item.anchor[1] * b.h;
                    b.x = area.x + cross_offset + cross_item - item.anchor[0] * b.w;
                }
            }
            cursor += size + flow_gap;
        }
        cross_offset += line_cross[line_idx] + cross_gap;
    }
    Ok(())
}

fn apply_grid_layout(
    ctx: &WorldRef,
    parent_id: &str,
    id: &str,
    area: Box2,
    b: &mut Box2,
) -> Result<(), String> {
    let layout = component(ctx, parent_id, "UIGridLayout").ok_or("grid layout missing")?;
    let (cell_x, cell_y) = udim2(&prop(
        ctx,
        &layout,
        "CellSize",
        json!({"$udim2":[{"$udim":[0,100]},{"$udim":[0,100]}]}),
    ))
    .ok_or("invalid grid CellSize")?;
    let (pad_x, pad_y) = udim2(&prop(
        ctx,
        &layout,
        "CellPadding",
        json!({"$udim2":[{"$udim":[0,5]},{"$udim":[0,5]}]}),
    ))
    .ok_or("invalid grid CellPadding")?;
    let cw = (cell_x[0] * area.w + cell_x[1] * area.scale).max(0.);
    let ch = (cell_y[0] * area.h + cell_y[1] * area.scale).max(0.);
    let px = pad_x[0] * area.w + pad_x[1] * area.scale;
    let py = pad_y[0] * area.h + pad_y[1] * area.scale;
    if ![cw, ch, px, py].iter().all(|v| v.is_finite()) {
        return Err("nonfinite grid geometry".into());
    }
    let direction = enum_value(&prop(
        ctx,
        &layout,
        "FillDirection",
        json!({"$enum":{"value":0}}),
    ))
    .unwrap_or(0);
    if direction != 0 && direction != 1 {
        return Err("invalid grid FillDirection".into());
    }
    let corner = enum_value(&prop(
        ctx,
        &layout,
        "StartCorner",
        json!({"$enum":{"value":0}}),
    ))
    .unwrap_or(0);
    let sort = enum_value(&prop(
        ctx,
        &layout,
        "SortOrder",
        json!({"$enum":{"value":2}}),
    ))
    .unwrap_or(2);
    if sort == 1 {
        return Err("UIGridLayout custom sorting is unavailable".into());
    }
    let limit = number(&prop(ctx, &layout, "FillDirectionMaxCells", json!(0)), 0.);
    if !limit.is_finite() || limit < 0. || limit.fract() != 0. || limit > 1_000_000. {
        return Err("invalid FillDirectionMaxCells".into());
    }
    struct Item {
        id: String,
        name: String,
        order: f64,
        span_x: usize,
        span_y: usize,
    }
    let mut items = Vec::new();
    for child in world::children(ctx, parent_id) {
        if !world::is_a(ctx, &child, "GuiObject")
            || !flag(&prop(ctx, &child, "Visible", json!(true)), true)
        {
            continue;
        }
        let (mut sx, mut sy) = (1usize, 1usize);
        if let Some(bounds) = component(ctx, &child, "UISizeConstraint") {
            let min = vector2(&prop(ctx, &bounds, "MinSize", json!({"$vector2":[0,0]})))
                .unwrap_or([0.; 2]);
            let span = |required: f64, size: f64, gap: f64| -> Result<usize, String> {
                if required <= size {
                    return Ok(1);
                }
                if size + gap <= 0. {
                    return Err("grid span requires positive cell stride".into());
                }
                let cells = ((required + gap) / (size + gap)).ceil();
                if !cells.is_finite() || cells > 1_000_000. {
                    return Err("grid span exceeds limit".into());
                }
                Ok(cells.max(1.) as usize)
            };
            sx = span(min[0] * area.scale, cw, px)?;
            sy = span(min[1] * area.scale, ch, py)?;
        }
        let name = ctx
            .borrow()
            .nodes
            .get(&child)
            .map(|n| n.name.clone())
            .unwrap_or_default();
        let order = number(&prop(ctx, &child, "LayoutOrder", json!(0)), 0.);
        items.push(Item {
            id: child,
            name,
            order,
            span_x: sx,
            span_y: sy,
        });
    }
    items.sort_by(|a, b| match sort {
        0 => a.name.cmp(&b.name),
        2 => a
            .order
            .partial_cmp(&b.order)
            .unwrap_or(std::cmp::Ordering::Equal),
        _ => std::cmp::Ordering::Equal,
    });
    let main_size = if direction == 0 { area.w } else { area.h };
    let cell_main = if direction == 0 { cw } else { ch };
    let gap_main = if direction == 0 { px } else { py };
    let total_main = items
        .iter()
        .map(|i| if direction == 0 { i.span_x } else { i.span_y })
        .sum::<usize>()
        .max(1);
    let fit = if cell_main + gap_main > 0. {
        ((main_size + gap_main) / (cell_main + gap_main))
            .floor()
            .max(1.) as usize
    } else {
        total_main
    };
    let mut capacity = fit.min(total_main).max(1);
    if limit > 0. {
        capacity = capacity.min(limit as usize)
    }
    let mut grid: Vec<Vec<bool>> = Vec::new();
    let mut coords = HashMap::<String, (usize, usize)>::new();
    let mut count = 0usize;
    for item in &items {
        let (sm, sc) = if direction == 0 {
            (item.span_x, item.span_y)
        } else {
            (item.span_y, item.span_x)
        };
        if sm > capacity {
            return Err("grid cell span exceeds fill capacity".into());
        }
        let mut found = None;
        let mut attempts = 0usize;
        'search: for cross in 0..1_000_000usize {
            for main in 0..=capacity - sm {
                attempts += 1;
                if attempts > 2_000_000 {
                    return Err("grid packing work limit exceeded".into());
                }
                let mut free = true;
                for y in cross..cross + sc {
                    for x in main..main + sm {
                        if grid
                            .get(y)
                            .and_then(|row| row.get(x))
                            .copied()
                            .unwrap_or(false)
                        {
                            free = false;
                            break;
                        }
                    }
                    if !free {
                        break;
                    }
                }
                if free {
                    found = Some((main, cross));
                    break 'search;
                }
            }
        }
        let (main, cross) = found.ok_or("grid cell count exceeds layout limit")?;
        for y in cross..cross + sc {
            while grid.len() <= y {
                grid.push(vec![false; capacity])
            }
            for x in main..main + sm {
                grid[y][x] = true
            }
        }
        count = count.max((cross + sc).saturating_mul(capacity));
        coords.insert(item.id.clone(), (main, cross));
    }
    if count > 2_000_000 {
        return Err("grid cell budget exceeded".into());
    }
    let mut used_main = 0usize;
    let mut used_cross = 0usize;
    for item in &items {
        if let Some((m, c)) = coords.get(&item.id) {
            let (sm, sc) = if direction == 0 {
                (item.span_x, item.span_y)
            } else {
                (item.span_y, item.span_x)
            };
            used_main = used_main.max(m + sm);
            used_cross = used_cross.max(c + sc)
        }
    }
    let columns = if direction == 0 {
        used_main
    } else {
        used_cross
    };
    let rows = if direction == 0 {
        used_cross
    } else {
        used_main
    };
    let width = if columns == 0 {
        0.
    } else {
        (columns as f64 - 1.) * (cw + px) + cw
    };
    let height = if rows == 0 {
        0.
    } else {
        (rows as f64 - 1.) * (ch + py) + ch
    };
    let h_align = enum_value(&prop(
        ctx,
        &layout,
        "HorizontalAlignment",
        json!({"$enum":{"value":1}}),
    ))
    .unwrap_or(1);
    let v_align = enum_value(&prop(
        ctx,
        &layout,
        "VerticalAlignment",
        json!({"$enum":{"value":1}}),
    ))
    .unwrap_or(1);
    let align = |mode: i64, free: f64| {
        if mode == 0 {
            free * 0.5
        } else if mode == 2 {
            free
        } else {
            0.
        }
    };
    let start_x = area.x + align(h_align, area.w - width);
    let start_y = area.y + align(v_align, area.h - height);
    if let Some(item) = items.iter().find(|item| item.id == id) {
        let (main, cross) = *coords.get(id).ok_or("missing grid coordinate")?;
        let (mut x, mut y) = (
            if direction == 0 { main } else { cross },
            if direction == 0 { cross } else { main },
        );
        let (span_x, span_y) = if direction == 0 {
            (item.span_x, item.span_y)
        } else {
            (item.span_y, item.span_x)
        };
        if corner & 1 != 0 {
            x = columns.saturating_sub(x + span_x)
        }
        if corner & 2 != 0 {
            y = rows.saturating_sub(y + span_y)
        }
        let own = b.scale / area.scale.max(f64::MIN_POSITIVE);
        b.w = cw * own;
        b.h = ch * own;
        if let Some(bounds) = component(ctx, id, "UISizeConstraint") {
            let min = vector2(&prop(ctx, &bounds, "MinSize", json!({"$vector2":[0,0]})))
                .unwrap_or([0.; 2]);
            let max = vector2(&prop(
                ctx,
                &bounds,
                "MaxSize",
                json!({"$vector2":[{"$number":"inf"},{"$number":"inf"}]}),
            ))
            .unwrap_or([f64::INFINITY; 2]);
            b.w = b.w.max(min[0] * b.scale).min(max[0] * b.scale);
            b.h = b.h.max(min[1] * b.scale).min(max[1] * b.scale);
        }
        if let Some(aspect) = component(ctx, id, "UIAspectRatioConstraint") {
            let ratio = number(&prop(ctx, &aspect, "AspectRatio", json!(1)), 1.);
            if ratio <= 0. {
                return Err("AspectRatio must be greater than zero".into());
            }
            if b.w == 0. || b.h == 0. {
                b.w = 0.;
                b.h = 0.
            } else if b.w / b.h > ratio {
                b.w = b.h * ratio
            } else {
                b.h = b.w / ratio
            }
        }
        b.x = start_x + x as f64 * (cw + px);
        b.y = start_y + y as f64 * (ch + py);
        b.rotation = 0.;
    }
    Ok(())
}

pub fn is_visible(ctx: &WorldRef, id: &str) -> bool {
    let mut cur = Some(id.to_owned());
    let mut seen = 0;
    while let Some(n) = cur {
        if seen >= MAX_DEPTH {
            return false;
        }
        seen += 1;
        if !flag(&prop(ctx, &n, "Visible", json!(true)), true)
            || !flag(&prop(ctx, &n, "Enabled", json!(true)), true)
        {
            return false;
        }
        let cls = class(ctx, &n);
        if cls == "CoreGui" {
            return true;
        }
        if cls == "PlayerGui" {
            return parent(ctx, &n) == local_player(ctx);
        }
        cur = parent(ctx, &n);
    }
    false
}
pub fn layout_bounds(ctx: &WorldRef, id: &str) -> Result<[f64; 5], String> {
    if !world::is_a(ctx, id, "GuiObject") {
        return Err("target is not a GuiObject".into());
    }
    let b = box_for(
        ctx,
        id,
        &display(ctx),
        &mut HashMap::new(),
        &mut HashSet::new(),
        0,
    )?;
    if !b.attached {
        return Err("GUI target is not attached to the local display".into());
    }
    Ok([b.x, b.y, b.w, b.h, b.rotation])
}
/// Apply physical pixel deltas to a local ScrollingFrame and return the
/// portion consumed after axis and canvas-bound clamping.
pub fn scroll_by(ctx: &WorldRef, id: &str, delta: [f64; 2]) -> Result<[f64; 2], String> {
    if class(ctx, id) != "ScrollingFrame" {
        return Err("scroll target must be a ScrollingFrame".into());
    }
    if delta.iter().any(|v| !v.is_finite() || v.abs() > 4096.) {
        return Err("scroll delta must be finite and bounded".into());
    }
    if !flag(&prop(ctx, id, "ScrollingEnabled", json!(true)), true) {
        return Ok([0., 0.]);
    }
    let mut cache = HashMap::new();
    let outer = box_for(ctx, id, &display(ctx), &mut cache, &mut HashSet::new(), 0)?;
    if !outer.attached || outer.scale <= 0. {
        return Ok([0., 0.]);
    }
    let canvas = scroll_canvas_extent(ctx, id, outer);
    let direction = prop(
        ctx,
        id,
        "ScrollingDirection",
        json!({"$enum":{"type":"ScrollingDirection","name":"XY","value":4}}),
    );
    let (enabled_x, enabled_y) = scrolling_axes(&direction);
    let bar = number(&prop(ctx, id, "ScrollBarThickness", json!(12)), 12.).max(0.) * outer.scale;
    let mut window = [outer.w, outer.h];
    let (mut hbar, mut vbar) = (false, false);
    for _ in 0..4 {
        window = [
            (outer.w - if vbar { bar } else { 0. }).max(0.),
            (outer.h - if hbar { bar } else { 0. }).max(0.),
        ];
        let next_h = enabled_x && canvas[0] > window[0];
        let next_v = enabled_y && canvas[1] > window[1];
        if next_h == hbar && next_v == vbar {
            break;
        }
        hbar = next_h;
        vbar = next_v;
    }
    let can_x = enabled_x && canvas[0] > window[0];
    let can_y = enabled_y && canvas[1] > window[1];
    let before =
        vector2(&prop(ctx, id, "CanvasPosition", json!({"$vector2":[0,0]}))).unwrap_or([0.; 2]);
    let after = [
        if can_x {
            (before[0] + delta[0] / outer.scale)
                .clamp(0., ((canvas[0] - window[0]) / outer.scale).max(0.))
        } else {
            0.
        },
        if can_y {
            (before[1] + delta[1] / outer.scale)
                .clamp(0., ((canvas[1] - window[1]) / outer.scale).max(0.))
        } else {
            0.
        },
    ];
    let consumed = [
        (after[0] - before[0]) * outer.scale,
        (after[1] - before[1]) * outer.scale,
    ];
    if after != before {
        let mut world = ctx.borrow_mut();
        let node = world.nodes.get_mut(id).ok_or("scroll target disappeared")?;
        node.local_overrides
            .insert("CanvasPosition".into(), json!({"$vector2":after}));
    }
    Ok(consumed)
}

fn get_gui_inset(
    lua: &mlua::Lua,
    ctx: &WorldRef,
    id: &str,
    _args: mlua::MultiValue,
) -> mlua::Result<mlua::MultiValue> {
    if class(ctx, id) != "GuiService" {
        return Err(err("GetGuiInset requires GuiService"));
    }
    let d = display(ctx);
    let insets = d
        .get("core_insets")
        .or_else(|| d.get("device_insets"))
        .and_then(Json::as_array)
        .filter(|a| a.len() == 4)
        .map(|a| {
            [
                number(&a[0], 0.),
                number(&a[1], 0.),
                number(&a[2], 0.),
                number(&a[3], 0.),
            ]
        })
        .unwrap_or([0.; 4]);
    Ok(mlua::MultiValue::from_vec(vec![
        super::values::from_json(lua, &json!({"$vector2":[insets[0],insets[1]]}))?,
        super::values::from_json(lua, &json!({"$vector2":[insets[2],insets[3]]}))?,
    ]))
}

fn apply_layout(
    lua: &mlua::Lua,
    ctx: &WorldRef,
    id: &str,
    _args: mlua::MultiValue,
) -> mlua::Result<mlua::MultiValue> {
    if !matches!(class(ctx, id).as_str(), "UIListLayout" | "UIGridLayout") {
        return Err(err("ApplyLayout requires UIListLayout or UIGridLayout"));
    }
    // Layout is derived lazily from the current world, so resolving content
    // size performs the same bounded layout pass used by all geometry reads.
    let _ = absolute_content_size(lua, ctx, id)?;
    Ok(mlua::MultiValue::new())
}
fn is_descendant(ctx: &WorldRef, id: &str, ancestor: &str) -> bool {
    let mut cur = Some(id.to_owned());
    let mut seen = 0;
    while let Some(node) = cur {
        if node == ancestor {
            return true;
        }
        if seen >= MAX_DEPTH {
            return false;
        }
        seen += 1;
        cur = parent(ctx, &node)
    }
    false
}
fn part_position(ctx: &WorldRef, id: &str) -> Option<Json> {
    let pos = prop(ctx, id, "Position", Json::Null);
    if pos.get("$vector3").is_some() {
        return Some(pos);
    }
    let frame = prop(ctx, id, "CFrame", Json::Null);
    let p = frame.get("$cframe").and_then(|v| v.get("position"))?;
    (p.as_array().is_some_and(|a| a.len() == 3)).then(|| json!({"$vector3":p}))
}
fn part_frame(ctx: &WorldRef, id: &str) -> Json {
    prop(ctx, id, "CFrame", Json::Null)
}
fn scroll_canvas_extent(ctx: &WorldRef, id: &str, outer: Box2) -> [f64; 2] {
    let (sx, sy) = udim2(&prop(
        ctx,
        id,
        "CanvasSize",
        json!({"$udim2":[{"$udim":[0,0]},{"$udim":[0,2]}]}),
    ))
    .unwrap_or(([0., 0.], [0., 2.]));
    let mut extent = [
        (sx[0] * outer.w + sx[1] * outer.scale).max(0.),
        (sy[0] * outer.h + sy[1] * outer.scale).max(0.),
    ];
    let automatic = enum_value(&prop(
        ctx,
        id,
        "AutomaticCanvasSize",
        json!({"$enum":{"value":0}}),
    ))
    .unwrap_or(0);
    if automatic == 0 {
        return extent;
    }
    let mut area = outer;
    let mut pad = [0.; 4];
    if let Some(padding) = component(ctx, id, "UIPadding") {
        pad[0] = udim(
            &prop(ctx, &padding, "PaddingLeft", json!({"$udim":[0,0]})),
            outer.w,
            outer.scale,
        )
        .unwrap_or(0.);
        pad[1] = udim(
            &prop(ctx, &padding, "PaddingRight", json!({"$udim":[0,0]})),
            outer.w,
            outer.scale,
        )
        .unwrap_or(0.);
        pad[2] = udim(
            &prop(ctx, &padding, "PaddingTop", json!({"$udim":[0,0]})),
            outer.h,
            outer.scale,
        )
        .unwrap_or(0.);
        pad[3] = udim(
            &prop(ctx, &padding, "PaddingBottom", json!({"$udim":[0,0]})),
            outer.h,
            outer.scale,
        )
        .unwrap_or(0.);
        area.x += pad[0];
        area.y += pad[2];
        area.w = (area.w - pad[0] - pad[1]).max(0.);
        area.h = (area.h - pad[2] - pad[3]).max(0.);
    }
    let mut content = [pad[0], pad[2]];
    if let Some(layout) = component(ctx, id, "UIListLayout") {
        let direction = enum_value(&prop(
            ctx,
            &layout,
            "FillDirection",
            json!({"$enum":{"value":1}}),
        ))
        .unwrap_or(1);
        let gap = udim(
            &prop(ctx, &layout, "Padding", json!({"$udim":[0,0]})),
            if direction == 0 { area.w } else { area.h },
            area.scale,
        )
        .unwrap_or(0.);
        let wraps = flag(&prop(ctx, &layout, "Wraps", json!(false)), false);
        let mut lines: Vec<(f64, f64)> = Vec::new();
        let mut main = 0.;
        let mut cross = 0.;
        for child in world::children(ctx, id) {
            if !world::is_a(ctx, &child, "GuiObject")
                || !flag(&prop(ctx, &child, "Visible", json!(true)), true)
            {
                continue;
            }
            let Some((sx, sy)) = read_udim2(ctx, &child, "Size") else {
                continue;
            };
            let scale = component(ctx, &child, "UIScale")
                .map(|item| number(&prop(ctx, &item, "Scale", json!(1)), 1.))
                .unwrap_or(1.);
            let w = (sx[0] * area.w + sx[1] * area.scale).max(0.) * scale;
            let h = (sy[0] * area.h + sy[1] * area.scale).max(0.) * scale;
            let m = if direction == 0 { w } else { h };
            let c = if direction == 0 { h } else { w };
            if wraps
                && !lines.is_empty()
                && main + gap + m > (if direction == 0 { area.w } else { area.h })
            {
                lines.push((main, cross));
                main = 0.;
                cross = 0.;
            }
            main += if main == 0. { m } else { gap + m };
            cross = cross.max(c);
        }
        if main > 0. {
            lines.push((main, cross))
        }
        let main_extent = lines.iter().map(|line| line.0).fold(0., f64::max);
        let cross_extent = lines.iter().map(|line| line.1).sum::<f64>()
            + gap * lines.len().saturating_sub(1) as f64;
        let (w, h) = if direction == 0 {
            (main_extent, cross_extent)
        } else {
            (cross_extent, main_extent)
        };
        content[0] = w + pad[0];
        content[1] = h + pad[2];
    } else if let Some(layout) = component(ctx, id, "UIGridLayout") {
        let (x, y) = udim2(&prop(
            ctx,
            &layout,
            "CellSize",
            json!({"$udim2":[{"$udim":[0,100]},{"$udim":[0,100]}]}),
        ))
        .unwrap_or(([0., 100.], [0., 100.]));
        let (px, py) = udim2(&prop(
            ctx,
            &layout,
            "CellPadding",
            json!({"$udim2":[{"$udim":[0,5]},{"$udim":[0,5]}]}),
        ))
        .unwrap_or(([0., 5.], [0., 5.]));
        let cw = (x[0] * area.w + x[1] * area.scale).max(0.);
        let ch = (y[0] * area.h + y[1] * area.scale).max(0.);
        let dx = px[0] * area.w + px[1] * area.scale;
        let dy = py[0] * area.h + py[1] * area.scale;
        let count = world::children(ctx, id)
            .into_iter()
            .filter(|child| {
                world::is_a(ctx, child, "GuiObject")
                    && flag(&prop(ctx, child, "Visible", json!(true)), true)
            })
            .count();
        let direction = enum_value(&prop(
            ctx,
            &layout,
            "FillDirection",
            json!({"$enum":{"value":0}}),
        ))
        .unwrap_or(0);
        let (main_size, cell, gap) = if direction == 0 {
            (area.w, cw, dx)
        } else {
            (area.h, ch, dy)
        };
        let capacity = if cell + gap > 0. {
            ((main_size + gap) / (cell + gap)).floor().max(1.) as usize
        } else {
            count.max(1)
        };
        let limit = number(&prop(ctx, &layout, "FillDirectionMaxCells", json!(0)), 0.);
        let cap = if limit > 0. {
            capacity.min(limit as usize)
        } else {
            capacity
        };
        let main_count = count.min(cap.max(1));
        let cross_count = count.div_ceil(cap.max(1));
        let (columns, rows) = if direction == 0 {
            (main_count, cross_count)
        } else {
            (cross_count, main_count)
        };
        content[0] = (if columns == 0 {
            0.
        } else {
            columns as f64 * cw + (columns - 1) as f64 * dx
        }) + pad[0];
        content[1] = (if rows == 0 {
            0.
        } else {
            rows as f64 * ch + (rows - 1) as f64 * dy
        }) + pad[2];
    } else {
        for child in world::children(ctx, id) {
            if !world::is_a(ctx, &child, "GuiObject")
                || !flag(&prop(ctx, &child, "Visible", json!(true)), true)
            {
                continue;
            }
            let (size_x, size_y) = match read_udim2(ctx, &child, "Size") {
                Some(v) => v,
                None => continue,
            };
            let (pos_x, pos_y) = match read_udim2(ctx, &child, "Position") {
                Some(v) => v,
                None => continue,
            };
            let own_scale = component(ctx, &child, "UIScale")
                .map(|scale| number(&prop(ctx, &scale, "Scale", json!(1)), 1.))
                .unwrap_or(1.);
            let constraint = enum_value(&prop(
                ctx,
                &child,
                "SizeConstraint",
                json!({"$enum":{"value":0}}),
            ))
            .unwrap_or(0);
            let w = (size_x[0] * (if constraint == 2 { area.h } else { area.w })
                + size_x[1] * area.scale)
                .max(0.)
                * own_scale;
            let h = (size_y[0] * (if constraint == 1 { area.w } else { area.h })
                + size_y[1] * area.scale)
                .max(0.)
                * own_scale;
            let anchor = vector2(&prop(ctx, &child, "AnchorPoint", json!({"$vector2":[0,0]})))
                .unwrap_or([0.; 2]);
            let x = pos_x[0] * area.w + pos_x[1] * area.scale - anchor[0] * w;
            let y = pos_y[0] * area.h + pos_y[1] * area.scale - anchor[1] * h;
            content[0] = content[0].max(pad[0] + x + w);
            content[1] = content[1].max(pad[2] + y + h);
        }
    }
    content[0] += pad[1];
    content[1] += pad[3];
    if automatic & 1 != 0 {
        extent[0] = extent[0].max(content[0])
    }
    if automatic & 2 != 0 {
        extent[1] = extent[1].max(content[1])
    }
    extent
}
fn project_parts(ctx: &WorldRef, ids: &[String], frame: &mut Json, bytes: &mut usize) {
    let workspace = ctx.borrow().services.get("Workspace").cloned();
    let Some(workspace) = workspace else { return };
    let local = local_player(ctx).and_then(|p| {
        prop(ctx, &p, "Character", Json::Null)
            .get("$ref")
            .and_then(Json::as_str)
            .map(ToOwned::to_owned)
    });
    let mut candidates: Vec<String> = ids
        .iter()
        .filter(|id| world::is_a(ctx, id, "BasePart") && is_descendant(ctx, id, &workspace))
        .cloned()
        .collect();
    candidates.sort_by_key(|id| {
        !local
            .as_ref()
            .is_some_and(|character| is_descendant(ctx, id, character))
    });
    for id in candidates {
        if frame["parts"]
            .as_array()
            .is_some_and(|a| a.len() >= MAX_PARTS)
            || *bytes > MAX_BYTES
        {
            increment(frame, "skippedParts");
            continue;
        }
        let Some(position) = part_position(ctx, &id) else {
            increment(frame, "skippedParts");
            continue;
        };
        let Some(size) = ctx
            .borrow()
            .nodes
            .get(&id)
            .map(|_| prop(ctx, &id, "Size", Json::Null))
            .filter(|v| v.get("$vector3").is_some())
        else {
            increment(frame, "skippedParts");
            continue;
        };
        let name = ctx
            .borrow()
            .nodes
            .get(&id)
            .map(|n| n.name.clone())
            .unwrap_or_default();
        let cls = class(ctx, &id);
        let own = local
            .as_ref()
            .is_some_and(|character| is_descendant(ctx, &id, character));
        let mut part = json!({"id":id,"name":name,"class":cls,"position":position,"size":size,
            "color":prop(ctx,&id,"Color",json!({"$color3":[0.64,0.64,0.64]})),
            "transparency":prop(ctx,&id,"Transparency",json!(0)),"localTransparency":prop(ctx,&id,"LocalTransparencyModifier",json!(0)),
            "material":prop(ctx,&id,"Material",json!(256)),"reflectance":prop(ctx,&id,"Reflectance",json!(0)),
            "castShadow":prop(ctx,&id,"CastShadow",json!(true)),"shape":prop(ctx,&id,"Shape",json!(1)),"own":own});
        let cf = part_frame(ctx, &id);
        if !cf.is_null() {
            part["frame"] = cf;
        }
        if cls == "MeshPart" {
            let mut mesh = json!({"class":"MeshPart","id":prop(ctx,&id,"MeshId",json!("")),"texture":prop(ctx,&id,"TextureID",json!("")),"size":prop(ctx,&id,"MeshSize",Json::Null)});
            if mesh["id"].as_str().unwrap_or("").is_empty() {
                mesh["id"] = prop(ctx, &id, "MeshContent", json!(""));
            }
            if mesh["texture"].as_str().unwrap_or("").is_empty() {
                mesh["texture"] = prop(ctx, &id, "TextureContent", json!(""));
            }
            part["mesh"] = mesh;
        }
        if own {
            part["characterPart"] = json!(name);
            if let Some(model) = parent(ctx, &id) {
                if world::children(ctx, &model)
                    .iter()
                    .any(|child| class(ctx, child) == "Humanoid")
                {
                    let mut clothing = json!({});
                    for child in world::children(ctx, &model) {
                        let class = class(ctx, &child);
                        let (key, property) = match class.as_str() {
                            "Shirt" => ("shirt", "ShirtTemplate"),
                            "Pants" => ("pants", "PantsTemplate"),
                            "ShirtGraphic" => ("graphic", "Graphic"),
                            _ => continue,
                        };
                        let value = prop(ctx, &child, property, json!(""));
                        if value.as_str().is_some_and(|v| !v.is_empty()) {
                            clothing[key] = value;
                        }
                    }
                    if clothing.as_object().is_some_and(|v| !v.is_empty()) {
                        part["clothing"] = clothing;
                    }
                }
            }
        }
        for child in world::children(ctx, &id) {
            let child_class = class(ctx, &child);
            if child_class == "SurfaceAppearance" && cls == "MeshPart" {
                let mut appearance = json!({"color":prop(ctx,&child,"Color",json!({"$color3":[1,1,1]})),"alphaMode":prop(ctx,&child,"AlphaMode",json!(0))});
                for (out, key) in [
                    ("colorMap", "ColorMap"),
                    ("normalMap", "NormalMap"),
                    ("roughnessMap", "RoughnessMap"),
                    ("metalnessMap", "MetalnessMap"),
                ] {
                    let value = prop(ctx, &child, key, json!(""));
                    let content = prop(ctx, &child, &format!("{key}Content"), json!(""));
                    let selected = if value.as_str().is_some_and(|v| !v.is_empty()) {
                        value
                    } else {
                        content
                    };
                    if selected.as_str().is_some_and(|v| !v.is_empty()) {
                        appearance[out] = selected;
                    }
                }
                part["appearance"] = appearance;
            }
            if matches!(
                child_class.as_str(),
                "SpecialMesh" | "BlockMesh" | "CylinderMesh"
            ) {
                part["mesh"] = json!({"class":child_class,"type":prop(ctx,&child,"MeshType",json!(0)),"id":prop(ctx,&child,"MeshId",json!("")),"texture":prop(ctx,&child,"TextureId",json!("")),"scale":prop(ctx,&child,"Scale",json!({"$vector3":[1,1,1]})),"offset":prop(ctx,&child,"Offset",json!({"$vector3":[0,0,0]})),"vertexColor":prop(ctx,&child,"VertexColor",json!({"$vector3":[1,1,1]}))});
            }
            if matches!(child_class.as_str(), "Decal" | "Texture")
                && part
                    .get("decals")
                    .and_then(Json::as_array)
                    .is_none_or(|a| a.len() < 12)
            {
                if !part.get("decals").is_some() {
                    part["decals"] = json!([])
                }
                part["decals"].as_array_mut().unwrap().push(json!({"id":child,"class":child_class,"texture":prop(ctx,&child,"Texture",json!("")),"face":prop(ctx,&child,"Face",json!(5)),"color":prop(ctx,&child,"Color3",json!({"$color3":[1,1,1]})),"transparency":prop(ctx,&child,"Transparency",json!(0)),"tileU":prop(ctx,&child,"StudsPerTileU",json!(2)),"tileV":prop(ctx,&child,"StudsPerTileV",json!(2))}));
            }
        }
        if own && name == "HumanoidRootPart" {
            frame["focus"] = position;
        }
        if matches!(cls.as_str(), "MeshPart" | "UnionOperation") {
            increment(frame, "meshBounds")
        }
        *bytes += part.to_string().len();
        if *bytes <= MAX_BYTES {
            frame["parts"].as_array_mut().unwrap().push(part)
        } else {
            increment(frame, "skippedParts")
        }
    }
}

/// Create GUI/engine bindings that are backed by data in the current snapshot.
pub fn install(lua: &mlua::Lua, ctx: &WorldRef) -> mlua::Result<()> {
    world::register_getter(ctx, "AbsoluteSize", absolute_size);
    world::register_getter(ctx, "AbsolutePosition", absolute_position);
    world::register_getter(ctx, "AbsoluteRotation", absolute_rotation);
    world::register_getter(ctx, "AbsoluteCanvasSize", absolute_canvas_size);
    world::register_getter(ctx, "AbsoluteWindowSize", absolute_window_size);
    world::register_getter(ctx, "CanvasPosition", canvas_position);
    world::register_getter(ctx, "AbsoluteContentSize", absolute_content_size);
    world::register_getter(ctx, "AbsoluteCellSize", absolute_cell_size);
    world::register_getter(ctx, "AbsoluteCellCount", absolute_cell_count);
    world::register_getter(ctx, "CursorPosition", cursor_position);
    world::register_getter(ctx, "SelectionStart", selection_start);
    world::register_setter(ctx, "CursorPosition", set_cursor_position);
    world::register_setter(ctx, "SelectionStart", set_selection_start);
    world::register_setter(ctx, "CanvasPosition", set_canvas_position);
    world::register_method(ctx, "GuiService", "GetGuiInset", get_gui_inset);
    world::register_method(ctx, "UIListLayout", "ApplyLayout", apply_layout);
    world::register_method(ctx, "UIGridLayout", "ApplyLayout", apply_layout);
    for key in [
        "AbsoluteSize",
        "AbsolutePosition",
        "AbsoluteRotation",
        "AbsoluteCanvasSize",
        "AbsoluteWindowSize",
        "AbsoluteContentSize",
        "AbsoluteCellSize",
        "AbsoluteCellCount",
    ] {
        world::register_setter(ctx, key, readonly_absolute);
    }
    // Font/line metadata is projected without guessing glyph widths.
    let _ = lua;
    Ok(())
}
fn textbox_value(ctx: &WorldRef, id: &str, key: &str) -> Option<i32> {
    if class(ctx, id) != "TextBox" || ctx.borrow().focused_textbox.as_deref() != Some(id) {
        return None;
    }
    let world = ctx.borrow();
    Some(if key == "CursorPosition" {
        world.focused_cursor
    } else {
        world.focused_anchor
    })
}
fn cursor_position(lua: &mlua::Lua, ctx: &WorldRef, id: &str) -> mlua::Result<Option<mlua::Value>> {
    textbox_value(ctx, id, "CursorPosition")
        .map(|v| super::values::from_json(lua, &json!(v)))
        .transpose()
}
fn selection_start(lua: &mlua::Lua, ctx: &WorldRef, id: &str) -> mlua::Result<Option<mlua::Value>> {
    textbox_value(ctx, id, "SelectionStart")
        .map(|v| super::values::from_json(lua, &json!(v)))
        .transpose()
}
fn set_text_position(
    ctx: &WorldRef,
    id: &str,
    key: &str,
    value: mlua::Value,
    lua: &mlua::Lua,
) -> mlua::Result<bool> {
    if class(ctx, id) != "TextBox" || ctx.borrow().focused_textbox.as_deref() != Some(id) {
        return Ok(false);
    }
    let json = super::values::to_json(lua, &value)?;
    let number = json
        .as_i64()
        .ok_or_else(|| err(format!("{key} must be an integer")))?;
    if key == "CursorPosition" && !(1..=4097).contains(&number) {
        return Err(err("CursorPosition must be in 1..4097"));
    }
    if key == "SelectionStart" && number != -1 && !(1..=4097).contains(&number) {
        return Err(err("SelectionStart must be -1 or in 1..4097"));
    }
    let mut world = ctx.borrow_mut();
    if key == "CursorPosition" {
        world.focused_cursor = number as i32
    } else {
        world.focused_anchor = number as i32
    };
    world.focused_editor_revision = world.focused_editor_revision.saturating_add(1);
    Ok(true)
}
fn set_cursor_position(
    lua: &mlua::Lua,
    ctx: &WorldRef,
    id: &str,
    value: mlua::Value,
) -> mlua::Result<bool> {
    set_text_position(ctx, id, "CursorPosition", value, lua)
}
fn set_selection_start(
    lua: &mlua::Lua,
    ctx: &WorldRef,
    id: &str,
    value: mlua::Value,
) -> mlua::Result<bool> {
    set_text_position(ctx, id, "SelectionStart", value, lua)
}
fn readonly_absolute(_: &mlua::Lua, _: &WorldRef, _: &str, _: mlua::Value) -> mlua::Result<bool> {
    Err(err("absolute GUI layout properties are read-only"))
}
fn set_canvas_position(
    lua: &mlua::Lua,
    ctx: &WorldRef,
    id: &str,
    value: mlua::Value,
) -> mlua::Result<bool> {
    if class(ctx, id) != "ScrollingFrame" {
        return Ok(false);
    }
    let json = super::values::to_json(lua, &value)?;
    let requested = vector2(&json).ok_or_else(|| err("CanvasPosition requires Vector2"))?;
    if requested.iter().any(|v| !v.is_finite()) {
        return Err(err("CanvasPosition must be finite"));
    }
    let mut cache = HashMap::new();
    let b = box_for(ctx, id, &display(ctx), &mut cache, &mut HashSet::new(), 0).map_err(err)?;
    if !flag(&prop(ctx, id, "ScrollingEnabled", json!(true)), true) || b.scale <= 0. {
        return Ok(true);
    }
    let canvas = scroll_canvas_extent(ctx, id, b);
    let axes = scrolling_axes(&prop(
        ctx,
        id,
        "ScrollingDirection",
        json!({"$enum":{"name":"XY","value":4}}),
    ));
    let bar = number(&prop(ctx, id, "ScrollBarThickness", json!(12)), 12.).max(0.) * b.scale;
    let mut window = [b.w, b.h];
    let (mut horizontal, mut vertical) = (false, false);
    for _ in 0..4 {
        window = [
            (b.w - if vertical { bar } else { 0. }).max(0.),
            (b.h - if horizontal { bar } else { 0. }).max(0.),
        ];
        let h = axes.0 && canvas[0] > window[0];
        let v = axes.1 && canvas[1] > window[1];
        if h == horizontal && v == vertical {
            break;
        }
        horizontal = h;
        vertical = v;
    }
    let x = if horizontal {
        requested[0].clamp(0., ((canvas[0] - window[0]) / b.scale).max(0.))
    } else {
        0.
    };
    let y = if vertical {
        requested[1].clamp(0., ((canvas[1] - window[1]) / b.scale).max(0.))
    } else {
        0.
    };
    ctx.borrow_mut()
        .nodes
        .get_mut(id)
        .ok_or_else(|| err("scroll target disappeared"))?
        .local_overrides
        .insert("CanvasPosition".into(), json!({"$vector2":[x,y]}));
    Ok(true)
}

fn display(ctx: &WorldRef) -> Json {
    ctx.borrow()
        .snapshot
        .get("display")
        .cloned()
        .unwrap_or_else(|| json!({"size":[0,0],"device_insets":[0,0,0,0],"core_insets":[0,0,0,0]}))
}
fn absolute_value(
    lua: &mlua::Lua,
    ctx: &WorldRef,
    id: &str,
    key: &str,
) -> mlua::Result<Option<mlua::Value>> {
    let mut cache = HashMap::new();
    let b = box_for(ctx, id, &display(ctx), &mut cache, &mut HashSet::new(), 0).map_err(err)?;
    let value = match key {
        "AbsoluteSize" => json!({"$vector2":[b.w,b.h]}),
        "AbsolutePosition" => json!({"$vector2":[b.x,b.y]}),
        "AbsoluteRotation" => json!(b.rotation),
        _ => return Ok(None),
    };
    super::values::from_json(lua, &value).map(Some)
}
fn absolute_size(lua: &mlua::Lua, ctx: &WorldRef, id: &str) -> mlua::Result<Option<mlua::Value>> {
    absolute_value(lua, ctx, id, "AbsoluteSize")
}
fn absolute_position(
    lua: &mlua::Lua,
    ctx: &WorldRef,
    id: &str,
) -> mlua::Result<Option<mlua::Value>> {
    absolute_value(lua, ctx, id, "AbsolutePosition")
}
fn absolute_rotation(
    lua: &mlua::Lua,
    ctx: &WorldRef,
    id: &str,
) -> mlua::Result<Option<mlua::Value>> {
    absolute_value(lua, ctx, id, "AbsoluteRotation")
}
fn display_vector(lua: &mlua::Lua, x: f64, y: f64) -> mlua::Result<mlua::Value> {
    super::values::from_json(lua, &json!({"$vector2":[x,y]}))
}
fn absolute_canvas_size(
    lua: &mlua::Lua,
    ctx: &WorldRef,
    id: &str,
) -> mlua::Result<Option<mlua::Value>> {
    if class(ctx, id) != "ScrollingFrame" {
        return Ok(None);
    }
    let b = layout_bounds(ctx, id).map_err(err)?;
    let box_ = Box2 {
        x: b[0],
        y: b[1],
        w: b[2],
        h: b[3],
        rotation: b[4],
        scale: {
            let mut c = HashMap::new();
            box_for(ctx, id, &display(ctx), &mut c, &mut HashSet::new(), 0)
                .map_err(err)?
                .scale
        },
        attached: true,
    };
    let canvas = scroll_canvas_extent(ctx, id, box_);
    display_vector(lua, canvas[0], canvas[1]).map(Some)
}
fn absolute_window_size(
    lua: &mlua::Lua,
    ctx: &WorldRef,
    id: &str,
) -> mlua::Result<Option<mlua::Value>> {
    if class(ctx, id) != "ScrollingFrame" {
        return Ok(None);
    }
    let b = layout_bounds(ctx, id).map_err(err)?;
    let canvas = scroll_canvas_extent(
        ctx,
        id,
        Box2 {
            x: b[0],
            y: b[1],
            w: b[2],
            h: b[3],
            rotation: b[4],
            scale: {
                let mut c = HashMap::new();
                box_for(ctx, id, &display(ctx), &mut c, &mut HashSet::new(), 0)
                    .map_err(err)?
                    .scale
            },
            attached: true,
        },
    );
    let (axes) = scrolling_axes(&prop(
        ctx,
        id,
        "ScrollingDirection",
        json!({"$enum":{"name":"XY","value":4}}),
    ));
    let bar = number(&prop(ctx, id, "ScrollBarThickness", json!(12)), 12.).max(0.);
    let can_x = axes.0 && canvas[0] > b[2];
    let can_y = axes.1 && canvas[1] > b[3];
    display_vector(
        lua,
        (b[2] - if can_y { bar } else { 0. }).max(0.),
        (b[3] - if can_x { bar } else { 0. }).max(0.),
    )
    .map(Some)
}
fn canvas_position(lua: &mlua::Lua, ctx: &WorldRef, id: &str) -> mlua::Result<Option<mlua::Value>> {
    if class(ctx, id) != "ScrollingFrame" {
        return Ok(None);
    }
    let _ = scroll_by(ctx, id, [0., 0.]);
    let pos =
        vector2(&prop(ctx, id, "CanvasPosition", json!({"$vector2":[0,0]}))).unwrap_or([0.; 2]);
    display_vector(lua, pos[0], pos[1]).map(Some)
}
fn absolute_content_size(
    lua: &mlua::Lua,
    ctx: &WorldRef,
    id: &str,
) -> mlua::Result<Option<mlua::Value>> {
    if !matches!(class(ctx, id).as_str(), "UIListLayout" | "UIGridLayout") {
        return Ok(None);
    }
    let container = parent(ctx, id).ok_or_else(|| err("layout has no parent"))?;
    let d = display(ctx);
    let mut cache = HashMap::new();
    let area = box_for(ctx, &container, &d, &mut cache, &mut HashSet::new(), 0).map_err(err)?;
    let mut width = 0f64;
    let mut height = 0f64;
    for child in world::children(ctx, &container) {
        if !world::is_a(ctx, &child, "GuiObject")
            || !flag(&prop(ctx, &child, "Visible", json!(true)), true)
        {
            continue;
        }
        let b = box_for(ctx, &child, &d, &mut cache, &mut HashSet::new(), 0).map_err(err)?;
        width = width.max(b.x - area.x + b.w);
        height = height.max(b.y - area.y + b.h);
    }
    display_vector(lua, width.max(0.), height.max(0.)).map(Some)
}
fn absolute_cell_size(
    lua: &mlua::Lua,
    ctx: &WorldRef,
    id: &str,
) -> mlua::Result<Option<mlua::Value>> {
    if class(ctx, id) != "UIGridLayout" {
        return Ok(None);
    }
    let container = parent(ctx, id).ok_or_else(|| err("grid has no parent"))?;
    let d = display(ctx);
    let mut cache = HashMap::new();
    let area = box_for(ctx, &container, &d, &mut cache, &mut HashSet::new(), 0).map_err(err)?;
    let (x, y) = udim2(&prop(
        ctx,
        id,
        "CellSize",
        json!({"$udim2":[{"$udim":[0,100]},{"$udim":[0,100]}]}),
    ))
    .unwrap_or(([0., 100.], [0., 100.]));
    display_vector(
        lua,
        (x[0] * area.w + x[1] * area.scale).max(0.),
        (y[0] * area.h + y[1] * area.scale).max(0.),
    )
    .map(Some)
}
fn absolute_cell_count(
    lua: &mlua::Lua,
    ctx: &WorldRef,
    id: &str,
) -> mlua::Result<Option<mlua::Value>> {
    if class(ctx, id) != "UIGridLayout" {
        return Ok(None);
    }
    let container = parent(ctx, id).ok_or_else(|| err("grid has no parent"))?;
    let d = display(ctx);
    let mut cache = HashMap::new();
    let area = box_for(ctx, &container, &d, &mut cache, &mut HashSet::new(), 0).map_err(err)?;
    let cell = absolute_cell_size_json(ctx, id, area);
    let direction = enum_value(&prop(
        ctx,
        id,
        "FillDirection",
        json!({"$enum":{"value":0}}),
    ))
    .unwrap_or(0);
    let (cw, ch) = cell;
    let (px, py) = udim2(&prop(
        ctx,
        id,
        "CellPadding",
        json!({"$udim2":[{"$udim":[0,5]},{"$udim":[0,5]}]}),
    ))
    .unwrap_or(([0., 5.], [0., 5.]));
    let dx = px[0] * area.w + px[1] * area.scale;
    let dy = py[0] * area.h + py[1] * area.scale;
    let mut count = 0usize;
    for child in world::children(ctx, &container) {
        if world::is_a(ctx, &child, "GuiObject")
            && flag(&prop(ctx, &child, "Visible", json!(true)), true)
        {
            count += 1
        }
    }
    let limit = number(&prop(ctx, id, "FillDirectionMaxCells", json!(0)), 0.);
    let main = if direction == 0 { area.w } else { area.h };
    let size = if direction == 0 { cw } else { ch };
    let gap = if direction == 0 { dx } else { dy };
    let mut columns = if size + gap > 0. {
        ((main + gap) / (size + gap)).floor().max(1.) as usize
    } else {
        count.max(1)
    };
    if limit > 0. {
        columns = columns.min(limit as usize)
    }
    columns = columns.min(count.max(1));
    let rows = count.div_ceil(columns.max(1));
    let (x, y) = if direction == 0 {
        (columns, rows)
    } else {
        (rows, columns)
    };
    display_vector(lua, x as f64, y as f64).map(Some)
}
fn absolute_cell_size_json(ctx: &WorldRef, id: &str, area: Box2) -> (f64, f64) {
    let (x, y) = udim2(&prop(
        ctx,
        id,
        "CellSize",
        json!({"$udim2":[{"$udim":[0,100]},{"$udim":[0,100]}]}),
    ))
    .unwrap_or(([0., 100.], [0., 100.]));
    (
        (x[0] * area.w + x[1] * area.scale).max(0.),
        (y[0] * area.h + y[1] * area.scale).max(0.),
    )
}

/// Build the v2 local frame consumed by the existing Rust render projection.
pub fn render(ctx: &WorldRef) -> mlua::Result<Json> {
    let world = ctx.borrow();
    let display =
        world.snapshot.get("display").cloned().unwrap_or_else(
            || json!({"size":[0,0],"device_insets":[0,0,0,0],"core_insets":[0,0,0,0]}),
        );
    let viewport = display.get("size").cloned().unwrap_or(json!([0, 0]));
    let sequence = world.sequence;
    let text_focus =
        json!({"id":super::input::focused_textbox(ctx),"revision":world.focused_text_revision});
    let text_editor = json!({"revision":world.focused_editor_revision,"cursor":world.focused_cursor.max(1),"anchor":if world.focused_anchor==0{-1}else{world.focused_anchor}});
    let ids = world::ordered_ids(ctx);
    drop(world);
    let mut frame = json!({"version":2,"sequence":sequence,"parts":[],"gui":[],"players":[],"viewport":viewport,"skippedParts":0,"skippedGui":0,"meshBounds":0,"terrainUnsupported":false});
    frame["drawings"] = super::drawing::scene(ctx);
    frame["textFocus"] = text_focus;
    frame["textEditor"] = text_editor;
    for id in &ids {
        if class(ctx, id) == "Terrain" {
            frame["terrainUnsupported"] = json!(true)
        }
        if class(ctx, id) == "Player"
            && frame["players"]
                .as_array()
                .is_some_and(|items| items.len() < MAX_PLAYERS)
        {
            let name = ctx
                .borrow()
                .nodes
                .get(id)
                .map(|n| n.name.clone())
                .unwrap_or_default();
            let display_name = prop(ctx, id, "DisplayName", json!(name));
            let is_local = ctx
                .borrow()
                .snapshot
                .get("local_player")
                .and_then(Json::as_str)
                == Some(id.as_str());
            frame["players"]
                .as_array_mut()
                .unwrap()
                .push(json!({"name":name,"displayName":display_name,"local":is_local}));
        }
    }
    let mut cache = HashMap::new();
    let mut bytes = 512usize;
    project_parts(ctx, &ids, &mut frame, &mut bytes);
    for id in ids {
        if frame["gui"].as_array().is_some_and(|a| a.len() >= MAX_GUI) || bytes > MAX_BYTES {
            increment(&mut frame, "skippedGui");
            continue;
        }
        if !world::is_a(ctx, &id, "GuiObject") || !is_visible(ctx, &id) {
            continue;
        }
        let cls = class(ctx, &id);
        let b = match box_for(ctx, &id, &display, &mut cache, &mut HashSet::new(), 0) {
            Ok(v) => v,
            Err(_) => {
                increment(&mut frame, "skippedGui");
                continue;
            }
        };
        if !b.attached || b.w <= 0. || b.h <= 0. {
            continue;
        }
        let name = ctx
            .borrow()
            .nodes
            .get(&id)
            .map(|n| n.name.clone())
            .unwrap_or_default();
        let text = prop(ctx, &id, "Text", json!(""));
        let mut gui = json!({"id":id,"class":cls,"name":name,"box":[b.x,b.y,b.w,b.h,b.rotation],
            "background":prop(ctx,&id,"BackgroundColor3",json!({"$color3":[1,1,1]})),
            "backgroundTransparency":prop(ctx,&id,"BackgroundTransparency",json!(0)),"z":prop(ctx,&id,"ZIndex",json!(1)),
            "text":text.clone(),"textColor":prop(ctx,&id,"TextColor3",json!({"$color3":[0,0,0]})),
            "textSize":prop(ctx,&id,"TextSize",json!(14)),"textScaled":prop(ctx,&id,"TextScaled",json!(false)),
            "textTransparency":prop(ctx,&id,"TextTransparency",json!(0)),"image":prop(ctx,&id,"Image",json!("")),
            "imageTransparency":prop(ctx,&id,"ImageTransparency",json!(0)),"interactive":class(ctx,&id)=="TextBox"||world::is_a(ctx,&id,"GuiButton"),
            "pointerTarget":if world::is_a(ctx,&id,"GuiButton"){id.clone()}else{String::new()},"textEditable":prop(ctx,&id,"TextEditable",json!(true))});
        if matches!(cls.as_str(), "TextLabel" | "TextButton" | "TextBox") {
            gui["textWrapped"] = prop(ctx, &id, "TextWrapped", json!(false));
            gui["lineHeight"] = prop(ctx, &id, "LineHeight", json!(1));
            gui["textScale"] = json!(b.scale);
            gui["multiLine"] = prop(ctx, &id, "MultiLine", json!(false));
            gui["textTruncate"] = enum_value(&prop(ctx, &id, "TextTruncate", Json::Null))
                .map(|v| json!(v))
                .unwrap_or(Json::Null);
            gui["maxVisibleGraphemes"] = prop(ctx, &id, "MaxVisibleGraphemes", json!(-1));
            let stroke_transparency =
                number(&prop(ctx, &id, "TextStrokeTransparency", json!(1)), 1.).clamp(0., 1.);
            if stroke_transparency < 1. {
                gui["legacyTextStroke"] = json!({"color":prop(ctx,&id,"TextStrokeColor3",Json::Null),"transparency":stroke_transparency});
            }
            for (source, target) in [
                ("TextXAlignment", "textXAlignment"),
                ("TextYAlignment", "textYAlignment"),
                ("TextDirection", "textDirection"),
            ] {
                let v = prop(ctx, &id, source, Json::Null);
                if let Some(v) = enum_value(&v) {
                    gui[target] = json!(v);
                }
            }
            let font = prop(ctx, &id, "FontFace", Json::Null);
            if font.get("$font").is_some() {
                gui["fontFace"] = font["$font"].clone();
            } else if !font.is_null() {
                gui["fontFace"] = font;
            }
            if cls == "TextBox" {
                gui["placeholderText"] = prop(ctx, &id, "PlaceholderText", json!(""));
                gui["placeholderColor"] = prop(ctx, &id, "PlaceholderColor3", Json::Null);
                gui["clearTextOnFocus"] = prop(ctx, &id, "ClearTextOnFocus", json!(true));
            }
            if let Some(constraint) = component(ctx, &id, "UITextSizeConstraint") {
                let min = prop(ctx, &constraint, "MinTextSize", Json::Null);
                let max = prop(ctx, &constraint, "MaxTextSize", Json::Null);
                if min.is_number() && max.is_number() {
                    gui["textSizeConstraint"] = json!([min, max]);
                }
            }
            if flag(&prop(ctx, &id, "RichText", json!(false)), false) {
                if let Some((content, runs)) = text.as_str().and_then(rich_text) {
                    gui["contentText"] = json!(content);
                    gui["richRuns"] = runs;
                } else {
                    gui["richTextUnavailable"] = json!(true);
                }
            }
        }
        let has_image = !gui["image"].as_str().unwrap_or("").is_empty()
            || (cls == "ImageButton"
                && ["HoverImage", "PressedImage"].iter().any(|key| {
                    !prop(ctx, &id, key, json!(""))
                        .as_str()
                        .unwrap_or("")
                        .is_empty()
                }));
        if matches!(cls.as_str(), "ImageLabel" | "ImageButton") && has_image {
            let scale_type = enum_value(&prop(ctx, &id, "ScaleType", Json::Null)).unwrap_or(0);
            gui["imageMode"] = json!(scale_type);
            gui["imageColor"] = prop(ctx, &id, "ImageColor3", Json::Null);
            gui["imagePixelated"] =
                json!(enum_value(&prop(ctx, &id, "ResampleMode", Json::Null)).unwrap_or(0) == 1);
            gui["imageOffset"] = prop(ctx, &id, "ImageRectOffset", Json::Null);
            gui["imageRect"] = prop(ctx, &id, "ImageRectSize", Json::Null);
            if scale_type == 1 {
                gui["imageSlice"] = prop(ctx, &id, "SliceCenter", Json::Null);
                gui["imageSliceScale"] =
                    json!(number(&prop(ctx, &id, "SliceScale", json!(1)), 1.) * b.scale);
            }
        }
        if cls == "ImageButton" {
            gui["imageHover"] = prop(ctx, &id, "HoverImage", json!(""));
            gui["imagePressed"] = prop(ctx, &id, "PressedImage", json!(""));
            gui["imageButtonEnabled"] = prop(ctx, &id, "Interactable", json!(true));
            if !gui["imageHover"].as_str().unwrap_or("").is_empty()
                || !gui["imagePressed"].as_str().unwrap_or("").is_empty()
            {
                gui["interactive"] = json!(true)
            }
        }
        if let Some(corner) = component(ctx, &id, "UICorner") {
            let radius = prop(ctx, &corner, "CornerRadius", json!({"$udim":[0,8]}));
            if let Some(radius) = udim(&radius, b.w.min(b.h), b.scale) {
                gui["cornerRadius"] = json!(radius.clamp(0., b.w.min(b.h) * 0.5));
            }
        }
        let mut ancestor = Some(id.clone());
        let mut display_order = 0;
        let mut depth = 0;
        while let Some(node) = ancestor {
            if depth >= MAX_DEPTH {
                break;
            }
            depth += 1;
            if class(ctx, &node) == "ScreenGui" {
                display_order = number(&prop(ctx, &node, "DisplayOrder", json!(0)), 0.) as i64;
                break;
            }
            ancestor = parent(ctx, &node)
        }
        gui["displayOrder"] = json!(display_order);
        let mut p = parent(ctx, &id);
        let mut clip: Option<[f64; 4]> = None;
        let mut depth = 0;
        let rotation_clips = ctx
            .borrow()
            .services
            .get("StarterGui")
            .is_some_and(|starter| {
                flag(
                    &prop(
                        ctx,
                        starter,
                        "ClipsDescendantsSupportsRotation",
                        json!(false),
                    ),
                    false,
                )
            });
        while b.rotation == 0. || rotation_clips {
            let Some(parent_id) = p else { break };
            if depth >= MAX_DEPTH {
                break;
            }
            depth += 1;
            if world::is_a(ctx, &parent_id, "GuiObject")
                && flag(
                    &prop(ctx, &parent_id, "ClipsDescendants", json!(false)),
                    false,
                )
            {
                if let Ok(bounds) = box_for(
                    ctx,
                    &parent_id,
                    &display,
                    &mut cache,
                    &mut HashSet::new(),
                    0,
                ) {
                    let rect = [bounds.x, bounds.y, bounds.x + bounds.w, bounds.y + bounds.h];
                    if bounds.rotation != 0. {
                        p = parent(ctx, &parent_id);
                        continue;
                    }
                    clip = Some(match clip {
                        Some(old) => [
                            old[0].max(rect[0]),
                            old[1].max(rect[1]),
                            old[2].min(rect[2]),
                            old[3].min(rect[3]),
                        ],
                        None => rect,
                    });
                }
            }
            p = parent(ctx, &parent_id);
        }
        if let Some([l, t, r, bottom]) = clip {
            gui["clip"] = json!([l, t, (r - l).max(0.), (bottom - t).max(0.)]);
        }
        if matches!(
            cls.as_str(),
            "Frame" | "TextLabel" | "TextButton" | "ImageLabel" | "ImageButton" | "ViewportFrame"
        ) {
            let mut outline: Option<(f64, Json)> = None;
            let mut borders: Vec<(f64, Json)> = Vec::new();
            let shortest = b.w.min(b.h);
            for stroke in world::children(ctx, &id)
                .into_iter()
                .filter(|child| class(ctx, child) == "UIStroke")
            {
                if !flag(&prop(ctx, &stroke, "Enabled", json!(true)), true) {
                    continue;
                }
                let thickness = number(&prop(ctx, &stroke, "Thickness", json!(1)), 1.);
                let transparency =
                    number(&prop(ctx, &stroke, "Transparency", json!(0)), 0.).clamp(0., 1.);
                if thickness <= 0. || transparency >= 1. {
                    continue;
                }
                let is_outline = matches!(cls.as_str(), "TextLabel" | "TextButton" | "TextBox")
                    && enum_value(&prop(
                        ctx,
                        &stroke,
                        "ApplyStrokeMode",
                        json!({"$enum":{"value":0}}),
                    ))
                    .unwrap_or(0)
                        == 0;
                let scaled = enum_value(&prop(
                    ctx,
                    &stroke,
                    "StrokeSizingMode",
                    json!({"$enum":{"value":0}}),
                ))
                .unwrap_or(0)
                    == 1;
                let z = number(&prop(ctx, &stroke, "ZIndex", json!(1)), 1.);
                let width = if scaled && !is_outline {
                    thickness * shortest
                } else {
                    thickness
                };
                let mut out = json!({"color":prop(ctx,&stroke,"Color",json!({"$color3":[0,0,0]})),"thickness":width.min(1000.),"transparency":transparency});
                if is_outline {
                    if outline.as_ref().is_some_and(|(old, _)| z <= *old) {
                        continue;
                    }
                    out["scaled"] = json!(scaled);
                    outline = Some((z, out));
                } else {
                    let position = enum_value(&prop(
                        ctx,
                        &stroke,
                        "BorderStrokePosition",
                        json!({"$enum":{"value":1}}),
                    ))
                    .unwrap_or(1);
                    let join = enum_value(&prop(
                        ctx,
                        &stroke,
                        "LineJoinMode",
                        json!({"$enum":{"value":0}}),
                    ))
                    .unwrap_or(0);
                    if position != 1 {
                        out["position"] = json!(position)
                    }
                    if join != 0 {
                        out["join"] = json!(join)
                    }
                    let insertion = borders.partition_point(|(order, _)| *order <= z);
                    borders.insert(insertion, (z, out));
                    if borders.len() > 8 {
                        borders.remove(0);
                    }
                }
            }
            if let Some((_, value)) = outline {
                gui["textOutline"] = value
            }
            if let Some((_, value)) = borders.last() {
                gui["stroke"] = value.clone();
                if borders.len() > 1 {
                    gui["borderStrokes"] =
                        json!(borders.iter().map(|(_, v)| v.clone()).collect::<Vec<_>>())
                }
            }
        }
        let mut ancestor = parent(ctx, &id);
        let mut depth = 0;
        while let Some(scroll) = ancestor {
            if depth >= MAX_DEPTH {
                break;
            }
            depth += 1;
            if class(ctx, &scroll) == "ScreenGui" {
                break;
            }
            if class(ctx, &scroll) == "ScrollingFrame"
                && flag(&prop(ctx, &scroll, "ScrollingEnabled", json!(true)), true)
            {
                if let Ok(view) =
                    box_for(ctx, &scroll, &display, &mut cache, &mut HashSet::new(), 0)
                {
                    gui["scrollTarget"] = json!(scroll);
                    gui["scrollViewport"] = json!([view.w, view.h]);
                    break;
                }
            }
            ancestor = parent(ctx, &scroll);
        }
        let size = gui.to_string().len();
        bytes += size;
        if bytes <= MAX_BYTES {
            frame["gui"].as_array_mut().unwrap().push(gui)
        }
    }
    for drawing in super::drawing::project_legacy_circles(ctx) {
        if frame["gui"].as_array().is_some_and(|a| a.len() < MAX_GUI) {
            frame["gui"].as_array_mut().unwrap().push(drawing);
        } else {
            increment(&mut frame, "skippedGui");
        }
    }
    Ok(frame)
}
