//! VM-local Drawing objects and bounded vector-scene export.
//!
//! Objects live in the Lua VM's local World. They are never server-owned
//! Instances and never access the operating system or a remote asset service.

use super::{numeric, source_helpers, values, world::WorldRef};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use mlua::{
    AnyUserData, Lua, MetaMethod, Result, UserData, UserDataFields, UserDataMethods, Value,
};
use serde_json::{json, Map, Value as Json};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

const MAX_OBJECTS: usize = 512;
const MAX_STRING: usize = 128 * 1024;
const MAX_SVG: usize = 1024 * 1024;
const MAX_SCENE_BYTES: usize = 1024 * 1024;
const MAX_SURFACE: f64 = 4096.;
const SCENE_KEY: &str = "drawing_scene";

type SceneRef = Rc<RefCell<Scene>>;

struct Scene {
    world: WorldRef,
    next_id: u64,
    objects: BTreeMap<u64, Object>,
}

#[derive(Clone)]
struct Object {
    kind: String,
    properties: Map<String, Json>,
    exists: bool,
}

#[derive(Clone)]
struct DrawingHandle {
    scene: SceneRef,
    id: u64,
}

fn error(message: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(message.into())
}

fn vector(x: f64, y: f64) -> Json {
    json!({"$vector2":[x,y]})
}

fn color(r: f64, g: f64, b: f64) -> Json {
    json!({"$color3":[r,g,b]})
}

fn defaults(kind: &str) -> Option<Map<String, Json>> {
    let mut properties = Map::from_iter([
        ("Visible".into(), json!(false)),
        ("ZIndex".into(), json!(1)),
        ("Transparency".into(), json!(1)),
        ("Color".into(), color(1., 1., 1.)),
    ]);
    match kind {
        "Line" => {
            properties.insert("From".into(), vector(0., 0.));
            properties.insert("To".into(), vector(0., 0.));
            properties.insert("Thickness".into(), json!(1));
        }
        "Triangle" | "Quad" => {
            for key in ["PointA", "PointB", "PointC"] {
                properties.insert(key.into(), vector(0., 0.));
            }
            if kind == "Quad" {
                properties.insert("PointD".into(), vector(0., 0.));
            }
            properties.insert("Thickness".into(), json!(1));
            properties.insert("Filled".into(), json!(false));
        }
        "Square" | "Circle" | "Text" | "Image" => {
            properties.insert("Position".into(), vector(500., 500.));
            match kind {
                "Square" => {
                    properties.insert("Size".into(), vector(25., 25.));
                    properties.insert("Thickness".into(), json!(1));
                    properties.insert("Filled".into(), json!(false));
                }
                "Circle" => {
                    properties.insert("Radius".into(), json!(5));
                    properties.insert("Thickness".into(), json!(1));
                    properties.insert("Filled".into(), json!(false));
                    properties.insert("NumSides".into(), json!(12));
                }
                "Image" => {
                    properties.insert("Size".into(), vector(64., 64.));
                    properties.insert("Data".into(), json!(""));
                    properties.insert("Rounding".into(), json!(0));
                }
                "Text" => {
                    properties.insert("Text".into(), json!(""));
                    properties.insert("Size".into(), json!(16));
                    properties.insert("Font".into(), json!(0));
                    properties.insert("Center".into(), json!(false));
                    properties.insert("Outline".into(), json!(false));
                    properties.insert("OutlineColor".into(), color(0., 0., 0.));
                    properties.insert("OutlineOpacity".into(), json!(1));
                }
                _ => unreachable!(),
            }
        }
        _ => return None,
    }
    Some(properties)
}

fn is_finite_number(value: &Value) -> Option<f64> {
    let number = match value {
        Value::Integer(value) => *value as f64,
        Value::Number(value) => *value,
        _ => return None,
    };
    number.is_finite().then_some(number)
}

fn property_kind(value: &Json) -> &'static str {
    if value.get("$vector2").is_some() {
        "Vector2"
    } else if value.get("$color3").is_some() {
        "Color3"
    } else if value.is_boolean() {
        "boolean"
    } else if value.is_number() {
        "number"
    } else if value.is_string() || value.get("$bytes_hex").is_some() {
        "string"
    } else {
        "unsupported"
    }
}

fn check_property(_lua: &Lua, key: &str, current: &Json, value: &Value) -> Result<Json> {
    let expected = property_kind(current);
    match expected {
        "number" => {
            let number = is_finite_number(value)
                .ok_or_else(|| error(format!("Drawing {key} must be a finite number")))?;
            if matches!(key, "Radius" | "Thickness" | "Rounding" | "Size") && number < 0. {
                return Err(error(format!("Drawing {key} must be nonnegative")));
            }
            if key == "Transparency" && !(0. ..=1.).contains(&number) {
                return Err(error("Transparency must be 0..1"));
            }
            if key == "OutlineOpacity" && !(0. ..=1.).contains(&number) {
                return Err(error("OutlineOpacity must be 0..1"));
            }
            if key == "NumSides" && (number.fract() != 0. || !(0. ..=1024.).contains(&number)) {
                return Err(error("Circle NumSides must be an integer from 0 to 1024"));
            }
            if key == "Font" && !(0. ..=3.).contains(&number) {
                return Err(error("unknown Drawing font"));
            }
            Ok(json!(number))
        }
        "boolean" => match value {
            Value::Boolean(value) => Ok(json!(value)),
            _ => Err(error(format!("Drawing {key} must be a boolean"))),
        },
        "string" => {
            let Value::String(value) = value else {
                return Err(error(format!("Drawing {key} must be a string")));
            };
            let bytes = value.as_bytes();
            if bytes.len() > MAX_STRING {
                return Err(error("Drawing string exceeds limit"));
            }
            match std::str::from_utf8(&bytes) {
                Ok(text) => Ok(Json::String(text.to_owned())),
                Err(_) if key == "Text" => Err(error("Drawing Text must be valid UTF-8")),
                Err(_) => {
                    let mut hex = String::with_capacity(bytes.len() * 2);
                    for byte in bytes.iter() {
                        use std::fmt::Write as _;
                        let _ = write!(&mut hex, "{byte:02x}");
                    }
                    Ok(json!({"$bytes_hex":hex}))
                }
            }
        }
        "Vector2" => {
            let Value::UserData(data) = value else {
                return Err(error(format!("Drawing {key} must be a Vector2")));
            };
            let v2 = data
                .borrow::<numeric::Vector2>()
                .map_err(|_| error(format!("Drawing {key} must be a Vector2")))?;
            if !v2.x.is_finite() || !v2.y.is_finite() || v2.x.abs() >= 1e7 || v2.y.abs() >= 1e7 {
                return Err(error("Drawing coordinates must be finite and bounded"));
            }
            Ok(vector(v2.x, v2.y))
        }
        "Color3" => {
            let Value::UserData(data) = value else {
                return Err(error(format!("Drawing {key} must be a Color3")));
            };
            let c3 = data
                .borrow::<numeric::Color3>()
                .map_err(|_| error(format!("Drawing {key} must be a Color3")))?;
            if !c3.r.is_finite() || !c3.g.is_finite() || !c3.b.is_finite() {
                return Err(error("Drawing color components must be finite"));
            }
            Ok(color(c3.r, c3.g, c3.b))
        }
        _ => Err(error("Drawing property has an unsupported declared type")),
    }
}

fn sync_world(scene: &Scene) {
    let objects = scene_json(scene);
    if objects.to_string().len() > MAX_SCENE_BYTES {
        return;
    }
    scene
        .world
        .borrow_mut()
        .effects
        .insert(SCENE_KEY.into(), objects);
}

fn scene_json(scene: &Scene) -> Json {
    let objects = scene
        .objects
        .iter()
        .filter(|(_, object)| object.exists)
        .map(|(id, object)| json!({"id":id,"kind":object.kind,"properties":object.properties}))
        .collect::<Vec<_>>();
    json!(objects)
}

pub(crate) fn scene(ctx: &WorldRef) -> Json {
    ctx.borrow()
        .effects
        .get(SCENE_KEY)
        .cloned()
        .unwrap_or_else(|| json!([]))
}

/// Republish this VM's persistent drawing scene before frame projection.
/// Session effects are drained after each command, so render calls invoke this
/// after scripts have run and before `gui::render` reads the local scene.
pub fn publish(lua: &Lua) -> Result<()> {
    let scene = scene_ref_from_lua(lua)?;
    sync_world(&scene.borrow());
    Ok(())
}

/// Remove the temporary effect after the frame JSON has copied its drawing
/// payload so it is not returned as a script mutation on the next command.
pub fn clear_published(ctx: &WorldRef) {
    ctx.borrow_mut().effects.remove(SCENE_KEY);
}

/// The old Drawing Circle is backed by a CoreGui Frame, UICorner, and
/// UIStroke. Project that exact supported surface into the regular GUI list;
/// the remaining primitive shapes are carried in the drawings scene payload.
pub(crate) fn project_legacy_circles(ctx: &WorldRef) -> Vec<Json> {
    let current_scene = scene(ctx);
    let Some(objects) = current_scene.as_array().cloned() else {
        return Vec::new();
    };
    objects
        .into_iter()
        .filter_map(|object| {
            if object.get("kind")?.as_str()? != "Circle" {
                return None;
            }
            let id = object.get("id")?.as_u64()?;
            let p = object.get("properties")?.as_object()?;
            if !p.get("Visible")?.as_bool()? {
                return None;
            }
            let (x, y) = point(p, "Position").ok()?;
            let radius = number(p, "Radius", 0.);
            let thickness = number(p, "Thickness", 0.);
            let transparency = number(p, "Transparency", 1.);
            let color = p.get("Color")?.clone();
            let filled = bool_property(p, "Filled");
            let z = p.get("ZIndex").cloned().unwrap_or(json!(1));
            let mut gui = json!({
                "id":format!("drawing:{id}"),
                "class":"Frame",
                "name":"DrawingCircle",
                "box":[x-radius,y-radius,radius*2.,radius*2.,0.],
                "background":color,
                "backgroundTransparency":if filled {1.-transparency} else {1.},
                "z":z,
                "text":"",
                "textColor":{"$color3":[0,0,0]},
                "textSize":14,
                "textScaled":false,
                "image":"",
                "interactive":false,
                "pointerTarget":"",
                "displayOrder":1000,
                "cornerRadius":radius
            });
            if thickness > 0. {
                gui["stroke"] = json!({
                    "color":color,
                    "thickness":thickness,
                    "transparency":1.-transparency,
                    "joins":"round",
                    "sizing":"fixed"
                });
            }
            Some(gui)
        })
        .collect()
}

fn get_property(handle: &DrawingHandle, key: &str) -> Result<Option<Json>> {
    let scene = handle.scene.borrow();
    if key == "__OBJECT_EXISTS" {
        return Ok(Some(json!(scene
            .objects
            .get(&handle.id)
            .is_some_and(|object| object.exists))));
    }
    let Some(object) = scene.objects.get(&handle.id) else {
        return Ok(None);
    };
    if !object.exists {
        return Ok(None);
    }
    if key == "TextBounds" && object.kind == "Text" {
        let text = object
            .properties
            .get("Text")
            .and_then(Json::as_str)
            .unwrap_or_default();
        if text.is_empty() {
            return Ok(Some(vector(0., 0.)));
        }
        let size = object
            .properties
            .get("Size")
            .and_then(Json::as_f64)
            .unwrap_or(0.);
        let size = if size > 0. { size } else { 16. };
        let mono = object.properties.get("Font").and_then(Json::as_u64) == Some(3);
        let advance = if mono { 0.6 } else { 0.55 };
        let mut width: f64 = 0.;
        let mut lines: f64 = 0.;
        for line in text.split('\n') {
            width = width.max(line.chars().count() as f64);
            lines += 1.;
        }
        return Ok(Some(vector(width * size * advance, lines * size)));
    }
    object
        .properties
        .get(key)
        .cloned()
        .map(Some)
        .ok_or_else(|| error(format!("unknown Drawing property: {key}")))
}

fn set_property(lua: &Lua, handle: &DrawingHandle, key: &str, value: &Value) -> Result<()> {
    let mut scene = handle.scene.borrow_mut();
    let old_value = {
        let object = scene
            .objects
            .get_mut(&handle.id)
            .ok_or_else(|| error("drawing object is destroyed or invalid"))?;
        if !object.exists {
            return Ok(());
        }
        let current = object
            .properties
            .get(key)
            .ok_or_else(|| error(format!("unknown Drawing property: {key}")))?;
        let new_value = check_property(lua, key, current, value)?;
        let old_value = current.clone();
        object.properties.insert(key.into(), new_value);
        old_value
    };
    if scene_json(&scene).to_string().len() > MAX_SCENE_BYTES {
        scene
            .objects
            .get_mut(&handle.id)
            .expect("object exists while setting property")
            .properties
            .insert(key.into(), old_value);
        return Err(error("Drawing scene exceeds 1 MiB"));
    }
    sync_world(&scene);
    Ok(())
}

fn remove(handle: &DrawingHandle) {
    let mut scene = handle.scene.borrow_mut();
    if scene.objects.remove(&handle.id).is_some() {
        sync_world(&scene);
    }
}

impl UserData for DrawingHandle {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_meta_field("__type", "DrawingObject");
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method(MetaMethod::Index, |lua, this, key: String| {
            if matches!(key.as_str(), "Remove" | "Destroy") {
                let handle = this.clone();
                return Ok(Value::Function(lua.create_function(
                    move |_, _: Value| {
                        remove(&handle);
                        Ok(())
                    },
                )?));
            }
            match get_property(this, &key)? {
                Some(value) => values::from_json(lua, &value),
                None => Ok(Value::Nil),
            }
        });
        methods.add_meta_method_mut(
            MetaMethod::NewIndex,
            |lua, this, (key, value): (String, Value)| set_property(lua, this, &key, &value),
        );
        methods.add_method("Remove", |_, this, ()| {
            remove(this);
            Ok(())
        });
        methods.add_method("Destroy", |_, this, ()| {
            remove(this);
            Ok(())
        });
        methods.add_meta_method(MetaMethod::ToString, |_, this, ()| {
            Ok(format!("DrawingObject({})", this.id))
        });
    }
}

fn new_object(lua: &Lua, scene_ref: &SceneRef, kind: &str) -> Result<AnyUserData> {
    let id = {
        let mut scene = scene_ref.borrow_mut();
        if scene.objects.len() >= MAX_OBJECTS {
            return Err(error("drawing object limit exceeded"));
        }
        let properties = defaults(kind).ok_or_else(|| error("unknown Drawing type"))?;
        scene.next_id = scene.next_id.saturating_add(1);
        let id = scene.next_id;
        scene.objects.insert(
            id,
            Object {
                kind: kind.into(),
                properties,
                exists: true,
            },
        );
        sync_world(&scene);
        id
    };
    lua.create_userdata(DrawingHandle {
        scene: scene_ref.clone(),
        id,
    })
}

fn scene_ref_from_lua(lua: &Lua) -> Result<SceneRef> {
    lua.app_data_ref::<SceneRef>()
        .map(|scene| scene.clone())
        .ok_or_else(|| error("Drawing is not installed"))
}

fn vector2(value: &Json) -> Result<(f64, f64)> {
    let pair = value
        .get("$vector2")
        .and_then(Json::as_array)
        .filter(|pair| pair.len() == 2)
        .ok_or_else(|| error("Drawing Vector2 value is malformed"))?;
    Ok((
        pair[0].as_f64().ok_or_else(|| error("invalid Vector2 X"))?,
        pair[1].as_f64().ok_or_else(|| error("invalid Vector2 Y"))?,
    ))
}

fn point(properties: &Map<String, Json>, key: &str) -> Result<(f64, f64)> {
    vector2(
        properties
            .get(key)
            .ok_or_else(|| error(format!("missing Drawing {key}")))?,
    )
}

fn number(properties: &Map<String, Json>, key: &str, fallback: f64) -> f64 {
    properties
        .get(key)
        .and_then(Json::as_f64)
        .unwrap_or(fallback)
}

fn bool_property(properties: &Map<String, Json>, key: &str) -> bool {
    properties.get(key).and_then(Json::as_bool).unwrap_or(false)
}

fn text_property<'a>(properties: &'a Map<String, Json>, key: &str) -> &'a str {
    properties
        .get(key)
        .and_then(Json::as_str)
        .unwrap_or_default()
}

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn color_css(value: &Json) -> Result<String> {
    let parts = value
        .get("$color3")
        .and_then(Json::as_array)
        .filter(|parts| parts.len() == 3)
        .ok_or_else(|| error("Drawing Color3 value is malformed"))?;
    let channels = parts
        .iter()
        .map(|v| {
            let v = v.as_f64().ok_or_else(|| error("invalid color component"))?;
            Ok((v.clamp(0., 1.) * 255.).round() as u8)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(format!(
        "rgb({},{},{})",
        channels[0], channels[1], channels[2]
    ))
}

fn svg_style(properties: &Map<String, Json>, filled: bool) -> Result<String> {
    let color = color_css(properties.get("Color").unwrap_or(&Json::Null))?;
    Ok(format!(
        " opacity=\"{}\" stroke=\"{}\" stroke-width=\"{}\" fill=\"{}\"",
        number(properties, "Transparency", 1.),
        color,
        number(properties, "Thickness", 0.),
        if filled { color.as_str() } else { "none" }
    ))
}

fn svg_for_object(lua: &Lua, scene: &Scene, id: u64, object: &Object) -> Result<String> {
    let p = &object.properties;
    let color = color_css(p.get("Color").unwrap_or(&Json::Null))?;
    let style = svg_style(p, bool_property(p, "Filled"))?;
    let output = match object.kind.as_str() {
        "Square" => {
            let (x, y) = point(p, "Position")?;
            let (w, h) = point(p, "Size")?;
            format!(
                "<rect x=\"{x}\" y=\"{y}\" width=\"{}\" height=\"{}\"{style}/>",
                w.max(0.),
                h.max(0.)
            )
        }
        "Line" => {
            let (x1, y1) = point(p, "From")?;
            let (x2, y2) = point(p, "To")?;
            format!("<line x1=\"{x1}\" y1=\"{y1}\" x2=\"{x2}\" y2=\"{y2}\"{style}/>")
        }
        "Circle" => {
            let (x, y) = point(p, "Position")?;
            let radius = number(p, "Radius", 0.);
            let sides = match number(p, "NumSides", 12.) as usize {
                0 => 64,
                n => n.max(3),
            };
            let mut points = Vec::with_capacity(sides);
            for i in 0..sides {
                let angle =
                    i as f64 * std::f64::consts::TAU / sides as f64 - std::f64::consts::FRAC_PI_2;
                points.push(format!(
                    "{},{}",
                    x + angle.cos() * radius,
                    y + angle.sin() * radius
                ));
            }
            format!("<polygon points=\"{}\"{style}/>", points.join(" "))
        }
        "Triangle" | "Quad" => {
            let mut points = Vec::new();
            for key in ["PointA", "PointB", "PointC", "PointD"] {
                if p.contains_key(key) {
                    let (x, y) = point(p, key)?;
                    points.push(format!("{x},{y}"));
                }
            }
            format!("<polygon points=\"{}\"{style}/>", points.join(" "))
        }
        "Text" => {
            let (x, y) = point(p, "Position")?;
            let font = match number(p, "Font", 0.) as usize {
                0 => "Arial, sans-serif",
                1 => "system-ui, sans-serif",
                2 => "IBM Plex Sans, sans-serif",
                _ => "monospace",
            };
            let anchor = if bool_property(p, "Center") {
                "middle"
            } else {
                "start"
            };
            let outline = if bool_property(p, "Outline") {
                format!(
                    " stroke=\"{}\" stroke-width=\"1\" stroke-opacity=\"{}\" paint-order=\"stroke\"",
                    color_css(p.get("OutlineColor").unwrap_or(&Json::Null))?,
                    number(p, "OutlineOpacity", 1.)
                )
            } else {
                String::new()
            };
            format!(
                "<text x=\"{x}\" y=\"{y}\" fill=\"{color}\" opacity=\"{}\" font-family=\"{font}\" font-size=\"{}\" text-anchor=\"{anchor}\" dominant-baseline=\"hanging\"{outline}>{}</text>",
                number(p, "Transparency", 1.),
                number(p, "Size", 16.),
                escape_xml(text_property(p, "Text"))
            )
        }
        "Image" => svg_image(lua, scene, id, p)?,
        _ => return Err(error("unsupported Drawing object kind")),
    };
    Ok(output)
}

fn image_bytes(lua: &Lua, value: &Json) -> Result<Vec<u8>> {
    if let Some(text) = value.as_str() {
        if text.starts_with("rbxasset://workspace/") {
            return source_helpers::resolve_workspace_asset(lua, text)
                .ok_or_else(|| error("Drawing Image workspace asset is unavailable"));
        }
        return Ok(text.as_bytes().to_vec());
    }
    if let Some(hex) = value.get("$bytes_hex").and_then(Json::as_str) {
        if hex.len() % 2 != 0 || hex.len() > MAX_STRING * 2 {
            return Err(error("Drawing Image byte string is invalid or too large"));
        }
        return hex
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| {
                let digit = |byte: u8| (byte as char).to_digit(16).map(|v| v as u8);
                Ok(
                    (digit(pair[0]).ok_or_else(|| error("invalid byte hex"))? << 4)
                        | digit(pair[1]).ok_or_else(|| error("invalid byte hex"))?,
                )
            })
            .collect();
    }
    Err(error("Drawing Image data is invalid"))
}

fn svg_image(lua: &Lua, _scene: &Scene, id: u64, p: &Map<String, Json>) -> Result<String> {
    let data = p
        .get("Data")
        .ok_or_else(|| error("Drawing Image data is missing"))?;
    let bytes = image_bytes(lua, data)?;
    let mime = if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF") {
        "image/gif"
    } else if bytes.is_empty() {
        return Ok(String::new());
    } else {
        return Err(error("Drawing Image requires PNG, JPEG, or GIF data"));
    };
    let (x, y) = point(p, "Position")?;
    let (w, h) = point(p, "Size")?;
    let w = w.max(0.);
    let h = h.max(0.);
    let rounding = number(p, "Rounding", 0.).min(w / 2.).min(h / 2.);
    let clip = if rounding > 0. && w > 0. && h > 0. {
        format!(
            "<defs><clipPath id=\"drawing-clip-{id}\" clipPathUnits=\"userSpaceOnUse\"><rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" rx=\"{rounding}\" ry=\"{rounding}\"/></clipPath></defs>"
        )
    } else {
        String::new()
    };
    let clip_attr = if rounding > 0. && w > 0. && h > 0. {
        format!(" clip-path=\"url(#drawing-clip-{id})\"")
    } else {
        String::new()
    };
    Ok(format!(
        "{clip}<image x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" opacity=\"{}\"{clip_attr} href=\"data:{mime};base64,{}\"/>",
        number(p, "Transparency", 1.),
        BASE64.encode(bytes)
    ))
}

fn svg(scene: &Scene, lua: &Lua, width: Option<f64>, height: Option<f64>) -> Result<String> {
    let (width, height) = (width.unwrap_or(800.), height.unwrap_or(600.));
    if !width.is_finite()
        || !height.is_finite()
        || width <= 0.
        || height <= 0.
        || width > MAX_SURFACE
        || height > MAX_SURFACE
    {
        return Err(error("invalid SVG surface size"));
    }
    let mut visible = scene
        .objects
        .iter()
        .filter(|(_, object)| {
            object.exists
                && object
                    .properties
                    .get("Visible")
                    .and_then(Json::as_bool)
                    .unwrap_or(false)
        })
        .collect::<Vec<_>>();
    visible.sort_by(|(id_a, a), (id_b, b)| {
        let za = a
            .properties
            .get("ZIndex")
            .and_then(Json::as_f64)
            .unwrap_or(1.);
        let zb = b
            .properties
            .get("ZIndex")
            .and_then(Json::as_f64)
            .unwrap_or(1.);
        za.total_cmp(&zb).then_with(|| id_a.cmp(id_b))
    });
    let mut result = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\">"
    );
    for (id, object) in visible {
        result.push_str(&svg_for_object(lua, scene, *id, object)?);
        if result.len() > MAX_SVG {
            return Err(error("SVG scene exceeds 1 MiB"));
        }
    }
    result.push_str("</svg>");
    if result.len() > MAX_SVG {
        return Err(error("SVG scene exceeds 1 MiB"));
    }
    Ok(result)
}

pub fn install(lua: &Lua, ctx: &WorldRef) -> Result<()> {
    let scene = Rc::new(RefCell::new(Scene {
        world: ctx.clone(),
        next_id: 0,
        objects: BTreeMap::new(),
    }));
    lua.set_app_data(scene.clone());
    let globals = lua.globals();
    let drawing = lua.create_table()?;
    let fonts = lua.create_table()?;
    fonts.set("UI", 0)?;
    fonts.set("System", 1)?;
    fonts.set("Plex", 2)?;
    fonts.set("Monospace", 3)?;
    fonts.set_readonly(true);
    drawing.set("Fonts", fonts)?;
    let scene_for_new = scene.clone();
    drawing.set(
        "new",
        lua.create_function(move |lua, kind: String| {
            if !matches!(
                kind.as_str(),
                "Square" | "Line" | "Circle" | "Text" | "Image" | "Triangle" | "Quad"
            ) {
                return Err(error("unknown Drawing type"));
            }
            new_object(lua, &scene_for_new, &kind)
        })?,
    )?;
    let scene_for_svg = scene.clone();
    drawing.set(
        "ToSVG",
        lua.create_function(move |lua, (width, height): (Option<f64>, Option<f64>)| {
            svg(&scene_for_svg.borrow(), lua, width, height)
        })?,
    )?;
    globals.set("Drawing", drawing)?;

    globals.set(
        "isrenderobj",
        lua.create_function(|_, object: Value| {
            let Value::UserData(object) = object else {
                return Ok(false);
            };
            Ok(object
                .borrow::<DrawingHandle>()
                .ok()
                .and_then(|handle| {
                    handle
                        .scene
                        .borrow()
                        .objects
                        .get(&handle.id)
                        .map(|object| object.exists)
                })
                .unwrap_or(false))
        })?,
    )?;
    globals.set(
        "getrenderproperty",
        lua.create_function(|lua, (value, key): (AnyUserData, String)| {
            let handle = value
                .borrow::<DrawingHandle>()
                .map_err(|_| error("drawing object is destroyed or invalid"))?;
            match get_property(&handle, &key)? {
                Some(value) => values::from_json(lua, &value),
                None => Ok(Value::Nil),
            }
        })?,
    )?;
    globals.set(
        "setrenderproperty",
        lua.create_function(
            |lua, (value, key, property): (AnyUserData, String, Value)| {
                let handle = value
                    .borrow::<DrawingHandle>()
                    .map_err(|_| error("drawing object is destroyed or invalid"))?;
                set_property(lua, &handle, &key, &property)
            },
        )?,
    )?;
    globals.set(
        "cleardrawcache",
        lua.create_function(move |_, ()| {
            let mut scene = scene.borrow_mut();
            scene.objects.clear();
            sync_world(&scene);
            Ok(())
        })?,
    )?;
    Ok(())
}

#[cfg(test)]
#[path = "drawing_tests.rs"]
mod tests;
