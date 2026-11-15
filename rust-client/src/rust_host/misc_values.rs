//! Remaining small Roblox value types and spatial query configuration values.
//!
//! This module deliberately keeps the host representation independent of the
//! renderer. Tagged JSON is the boundary used by snapshots and the world query
//! layer.
use super::world::{self, World, WorldRef};
use crate::rust_host::{
    numeric::Vector2,
    values::{self, Codec},
};
use mlua::{
    AnyUserData, Lua, MetaMethod, Result, UserData, UserDataFields, UserDataMethods, Value, Vector,
};
use serde_json::{json, Value as Json};
use std::{
    cell::RefCell,
    rc::{Rc, Weak},
};

fn err(s: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(s.into())
}
fn f32v(n: f64, what: &str) -> Result<f32> {
    if !n.is_finite() || n.abs() > f32::MAX as f64 {
        Err(err(format!("{what} must be a finite float")))
    } else {
        Ok(n as f32)
    }
}
fn push_v3(_lua: &Lua, v: [f32; 3]) -> Value {
    Value::Vector(Vector::new(v[0], v[1], v[2]))
}
fn vec3(v: Value) -> Result<[f32; 3]> {
    match v {
        Value::Vector(v) => Ok([v.x(), v.y(), v.z()]),
        _ => Err(err("Vector3 expected")),
    }
}
fn tag_body<'a>(j: &'a Json, tag: &str) -> Result<&'a Json> {
    if j.as_object().is_some_and(|m| m.len() == 1) {
        j.get(tag).ok_or_else(|| err(format!("expected {tag} tag")))
    } else {
        Err(err(format!("expected {tag} tag")))
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Region3 {
    min: [f32; 3],
    max: [f32; 3],
}
impl UserData for Region3 {
    fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
        f.add_meta_field("__type", "Region3");
        f.add_field_method_get("Size", |_, v| {
            Ok(push_v3_dummy(std::array::from_fn(|i| v.max[i] - v.min[i])))
        });
        f.add_field_method_get("CFrame", |lua, v| {
            let c: mlua::Table = lua.globals().get("CFrame")?;
            let new: mlua::Function = c.get("new")?;
            new.call::<Value>((
                (v.min[0] as f64 + v.max[0] as f64) * 0.5,
                (v.min[1] as f64 + v.max[1] as f64) * 0.5,
                (v.min[2] as f64 + v.max[2] as f64) * 0.5,
            ))
        });
    }
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_meta_method(MetaMethod::NewIndex, |_, _, (k, _): (Value, Value)| {
            Err::<(), _>(err(format!("Region3 values are immutable ({k:?})")))
        });
        m.add_meta_method(MetaMethod::Eq, |_, a, b: AnyUserData| {
            Ok(b.borrow::<Self>().map(|b| a == &*b).unwrap_or(false))
        });
        m.add_meta_method(MetaMethod::ToString, |_, v, ()| {
            Ok(format!(
                "{{{:.6}, {:.6}, {:.6}}}, {{{:.6}, {:.6}, {:.6}}}",
                v.min[0], v.min[1], v.min[2], v.max[0], v.max[1], v.max[2]
            ))
        });
        m.add_method("ExpandToGrid", |lua, v, res: f64| {
            let r = f32v(res, "resolution")?;
            if r <= 0.0 {
                return Err(err("positive resolution expected"));
            }
            let mut lo = [0.; 3];
            let mut hi = [0.; 3];
            for i in 0..3 {
                if v.min[i] > v.max[i] {
                    return Err(err(
                        "ExpandToGrid requires nonnegative Region3 size components",
                    ));
                }
                lo[i] = f32v(
                    (v.min[i] as f64 / r as f64).floor() * r as f64,
                    "expanded minimum",
                )?;
                hi[i] = f32v(
                    (v.max[i] as f64 / r as f64).ceil() * r as f64,
                    "expanded maximum",
                )?
            }
            lua.create_userdata(Self { min: lo, max: hi })
        });
    }
}
fn push_v3_dummy(v: [f32; 3]) -> Vector {
    Vector::new(v[0], v[1], v[2])
}
fn dec_region(lua: &Lua, j: &Json) -> Result<Value> {
    let b = tag_body(j, "$region3")?;
    let a = b
        .as_array()
        .filter(|x| x.len() == 2)
        .ok_or_else(|| err("invalid Region3 component count"))?;
    fn part(j: &Json) -> Result<[f32; 3]> {
        let a = j
            .as_array()
            .filter(|x| x.len() == 3)
            .ok_or_else(|| err("invalid Region3 vector"))?;
        Ok([
            f32v(
                a[0].as_f64().ok_or_else(|| err("number expected"))?,
                "component",
            )?,
            f32v(
                a[1].as_f64().ok_or_else(|| err("number expected"))?,
                "component",
            )?,
            f32v(
                a[2].as_f64().ok_or_else(|| err("number expected"))?,
                "component",
            )?,
        ])
    }
    Ok(Value::UserData(lua.create_userdata(Region3 {
        min: part(&a[0])?,
        max: part(&a[1])?,
    })?))
}
fn enc_region(u: &AnyUserData) -> Result<Option<Json>> {
    let Ok(v) = u.borrow::<Region3>() else {
        return Ok(None);
    };
    Ok(Some(json!({"$region3":[v.min,v.max]})))
}
pub fn region_bounds(u: &AnyUserData) -> Option<([f64; 3], [f64; 3])> {
    let v = u.borrow::<Region3>().ok()?;
    Some((v.min.map(f64::from), v.max.map(f64::from)))
}
pub fn region3_bounds(value: &Value) -> Option<([f64; 3], [f64; 3])> {
    let Value::UserData(userdata) = value else {
        return None;
    };
    region_bounds(userdata)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ray {
    origin: [f32; 3],
    direction: [f32; 3],
}
impl UserData for Ray {
    fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
        f.add_meta_field("__type", "Ray");
        f.add_field_method_get("Origin", |_, v| Ok(push_v3_dummy(v.origin)));
        f.add_field_method_get("Direction", |_, v| Ok(push_v3_dummy(v.direction)));
        f.add_field_method_get("Unit", |lua, v| {
            let n = v
                .direction
                .iter()
                .map(|x| (*x as f64).powi(2))
                .sum::<f64>()
                .sqrt();
            if n == 0.0 {
                return Err(err("cannot normalize a zero Ray direction"));
            }
            lua.create_userdata(Self {
                origin: v.origin,
                direction: v.direction.map(|x| (x as f64 / n) as f32),
            })
        });
    }
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_meta_method(MetaMethod::NewIndex, |_, _, _: (Value, Value)| {
            Err::<(), _>(err("Ray values are immutable"))
        });
        m.add_meta_method(MetaMethod::Eq, |_, a, b: AnyUserData| {
            Ok(b.borrow::<Self>().map(|b| a == &*b).unwrap_or(false))
        });
        m.add_meta_method(MetaMethod::ToString, |_, v, ()| {
            Ok(format!(
                "{{{:.6}, {:.6}, {:.6}}}, {{{:.6}, {:.6}, {:.6}}}",
                v.origin[0],
                v.origin[1],
                v.origin[2],
                v.direction[0],
                v.direction[1],
                v.direction[2]
            ))
        });
        m.add_method("ClosestPoint", |lua, v, p: Value| {
            let p = vec3(p)?;
            let d = v.direction.map(|x| x as f64);
            let delta = std::array::from_fn::<_, 3, _>(|i| p[i] as f64 - v.origin[i] as f64);
            let t = (0..3).map(|i| delta[i] * d[i]).sum::<f64>().max(0.0);
            Ok(push_v3(
                lua,
                std::array::from_fn(|i| (v.origin[i] as f64 + t * d[i]) as f32),
            ))
        });
        m.add_method("Distance", |_, v, p: Value| {
            let p = vec3(p)?;
            let d = v.direction.map(|x| x as f64);
            let delta = std::array::from_fn::<_, 3, _>(|i| p[i] as f64 - v.origin[i] as f64);
            let t = (0..3).map(|i| delta[i] * d[i]).sum::<f64>().max(0.0);
            Ok((0..3)
                .map(|i| (p[i] as f64 - v.origin[i] as f64 - t * d[i]).powi(2))
                .sum::<f64>()
                .sqrt())
        });
    }
}
fn dec_ray(lua: &Lua, j: &Json) -> Result<Value> {
    let b = tag_body(j, "$ray")?;
    let a = b
        .as_array()
        .filter(|x| x.len() == 2)
        .ok_or_else(|| err("invalid Ray"))?;
    fn v(j: &Json) -> Result<[f32; 3]> {
        let x = j
            .as_array()
            .filter(|x| x.len() == 3)
            .ok_or_else(|| err("invalid Ray vector"))?;
        Ok([
            f32v(
                x[0].as_f64().ok_or_else(|| err("number expected"))?,
                "component",
            )?,
            f32v(
                x[1].as_f64().ok_or_else(|| err("number expected"))?,
                "component",
            )?,
            f32v(
                x[2].as_f64().ok_or_else(|| err("number expected"))?,
                "component",
            )?,
        ])
    }
    Ok(Value::UserData(lua.create_userdata(Ray {
        origin: v(&a[0])?,
        direction: v(&a[1])?,
    })?))
}
fn enc_ray(u: &AnyUserData) -> Result<Option<Json>> {
    let Ok(v) = u.borrow::<Ray>() else {
        return Ok(None);
    };
    Ok(Some(json!({"$ray":[v.origin,v.direction]})))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Font {
    family: String,
    weight: i32,
    style: i32,
}
impl UserData for Font {
    fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
        f.add_meta_field("__type", "Font");
        f.add_field_method_get("Family", |_, v| Ok(v.family.clone()));
        f.add_field_method_set("Family", |_, v, s: String| {
            if s.len() > 4096 || s.as_bytes().contains(&0) {
                return Err(err("invalid Font family"));
            }
            v.family = s;
            Ok(())
        });
        f.add_field_method_get("Weight", |lua, v| enum_value(lua, "FontWeight", v.weight));
        f.add_field_method_set("Weight", |lua, v, x: Value| {
            v.weight = read_enum(&lua, x, "FontWeight")?;
            if !(100..=900).contains(&v.weight) || v.weight % 100 != 0 {
                return Err(err("invalid FontWeight"));
            }
            Ok(())
        });
        f.add_field_method_get("Style", |lua, v| enum_value(lua, "FontStyle", v.style));
        f.add_field_method_set("Style", |lua, v, x: Value| {
            v.style = read_enum(&lua, x, "FontStyle")?;
            if !(0..=1).contains(&v.style) {
                return Err(err("invalid FontStyle"));
            }
            Ok(())
        });
        f.add_field_method_get("Bold", |_, v| Ok(v.weight >= 600));
        f.add_field_method_set("Bold", |_, v, b: bool| {
            v.weight = if b { 700 } else { 400 };
            Ok(())
        });
    }
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_meta_method(MetaMethod::Eq, |_, a, b: AnyUserData| {
            Ok(b.borrow::<Self>().map(|b| a == &*b).unwrap_or(false))
        });
        m.add_meta_method(MetaMethod::ToString, |_, v, ()| {
            Ok(format!(
                "Font {{ Family = {}, Weight = {}, Style = {} }}",
                v.family, v.weight, v.style
            ))
        });
    }
}
const LEGACY_FONTS: &[(&str, i32, i32)] = &[
    ("LegacyArial", 400, 0),
    ("Arimo", 400, 0),
    ("Arimo", 700, 0),
    ("SourceSansPro", 400, 0),
    ("SourceSansPro", 700, 0),
    ("SourceSansPro", 300, 0),
    ("SourceSansPro", 400, 1),
    ("AccanthisADFStd", 400, 0),
    ("Guru", 400, 0),
    ("ComicNeueAngular", 400, 0),
    ("Inconsolata", 400, 0),
    ("HighwayGothic", 400, 0),
    ("Zekton", 400, 0),
    ("PressStart2P", 400, 0),
    ("Balthazar", 400, 0),
    ("RomanAntique", 400, 0),
    ("SourceSansPro", 600, 0),
    ("Montserrat", 400, 0),
    ("Montserrat", 500, 0),
    ("Montserrat", 700, 0),
    ("Montserrat", 900, 0),
    ("AmaticSC", 400, 0),
    ("Bangers", 400, 0),
    ("Creepster", 400, 0),
    ("DenkOne", 400, 0),
    ("Fondamento", 400, 0),
    ("FredokaOne", 400, 0),
    ("GrenzeGotisch", 400, 0),
    ("IndieFlower", 400, 0),
    ("JosefinSans", 400, 0),
    ("Jura", 400, 0),
    ("Kalam", 400, 0),
    ("LuckiestGuy", 400, 0),
    ("Merriweather", 400, 0),
    ("Michroma", 400, 0),
    ("Nunito", 400, 0),
    ("Oswald", 400, 0),
    ("PatrickHand", 400, 0),
    ("PermanentMarker", 400, 0),
    ("Roboto", 400, 0),
    ("RobotoCondensed", 400, 0),
    ("RobotoMono", 400, 0),
    ("Sarpanch", 400, 0),
    ("SpecialElite", 400, 0),
    ("TitilliumWeb", 400, 0),
    ("Ubuntu", 400, 0),
    ("BuilderSans", 400, 0),
    ("BuilderSans", 500, 0),
    ("BuilderSans", 700, 0),
    ("BuilderSans", 800, 0),
    ("Arimo", 400, 0),
    ("Arimo", 700, 0),
];
fn enum_value(lua: &Lua, ty: &str, n: i32) -> Result<Value> {
    values::from_json(lua, &json!({"$enum":{"type":ty,"value":n}}))
}
fn read_enum(_lua: &Lua, v: Value, ty: &str) -> Result<i32> {
    let Value::UserData(item) = v else {
        return Err(err(format!("Enum.{ty} item expected")));
    };
    if !super::enums::enum_item_matches(&item, ty)? {
        return Err(err(format!("Enum.{ty} item expected")));
    }
    super::enums::enum_item_value(&item)
}
fn dec_font(lua: &Lua, j: &Json) -> Result<Value> {
    let b = tag_body(j, "$font")?;
    let family = b
        .get("family")
        .and_then(Json::as_str)
        .ok_or_else(|| err("invalid Font family"))?;
    let weight = b
        .get("weight")
        .and_then(Json::as_i64)
        .ok_or_else(|| err("invalid Font weight"))? as i32;
    let style = b
        .get("style")
        .and_then(Json::as_i64)
        .ok_or_else(|| err("invalid Font style"))? as i32;
    if family.len() > 4096
        || family.as_bytes().contains(&0)
        || !(100..=900).contains(&weight)
        || weight % 100 != 0
        || !(0..=1).contains(&style)
    {
        return Err(err("invalid Font components"));
    }
    Ok(Value::UserData(lua.create_userdata(Font {
        family: family.into(),
        weight,
        style,
    })?))
}
fn enc_font(u: &AnyUserData) -> Result<Option<Json>> {
    let Ok(v) = u.borrow::<Font>() else {
        return Ok(None);
    };
    Ok(Some(
        json!({"$font":{"family":v.family,"weight":v.weight,"style":v.style}}),
    ))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DirectionSet {
    bits: u8,
    axes: bool,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct AxesValue(DirectionSet);
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct FacesValue(DirectionSet);
macro_rules! impl_direction_userdata {
    ($wrapper:ident,$name:literal,$is_axes:expr) => {
        impl UserData for $wrapper {
            fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
                f.add_meta_field("__type", $name);
                for (field, index, axes_only) in [
                    ("X", 0, true),
                    ("Y", 1, true),
                    ("Z", 2, true),
                    ("Right", 0, false),
                    ("Top", 1, false),
                    ("Back", 2, false),
                    ("Left", 3, false),
                    ("Bottom", 4, false),
                    ("Front", 5, false),
                ] {
                    f.add_field_method_get(field, move |_, v| {
                        if axes_only && !$is_axes {
                            return Err(err("unknown direction-set member"));
                        }
                        let bit = if $is_axes { index % 3 } else { index };
                        Ok(v.0.bits & (1 << bit) != 0)
                    });
                }
            }
            fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
                m.add_meta_method(MetaMethod::NewIndex, |_, _, _: (Value, Value)| {
                    Err::<(), _>(err("Axes and Faces are immutable"))
                });
                m.add_meta_method(MetaMethod::Eq, |_, a, b: AnyUserData| {
                    Ok(b.borrow::<Self>().map(|b| a == &*b).unwrap_or(false))
                });
                m.add_meta_method(MetaMethod::ToString, |_, v, ()| {
                    let names = if $is_axes {
                        ["X", "Y", "Z", "", "", ""]
                    } else {
                        ["Right", "Top", "Back", "Left", "Bottom", "Front"]
                    };
                    let count = if $is_axes { 3 } else { 6 };
                    Ok((0..count)
                        .filter(|i| v.0.bits & (1 << i) != 0)
                        .map(|i| names[i])
                        .collect::<Vec<_>>()
                        .join(", "))
                });
            }
        }
    };
}
impl_direction_userdata!(AxesValue, "Axes", true);
impl_direction_userdata!(FacesValue, "Faces", false);

#[derive(Clone, Debug)]
pub struct CellId {
    is_nil: bool,
    location: [f32; 3],
}
impl UserData for CellId {
    fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
        f.add_meta_field("__type", "CellId");
        f.add_field_method_get("IsNil", |_, v| Ok(v.is_nil));
        f.add_field_method_get("Location", |_, v| {
            Ok(Vector::new(v.location[0], v.location[1], v.location[2]))
        });
        f.add_field_method_get("TerrainPart", |_, _| Ok(Value::Nil));
    }
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_meta_method(MetaMethod::NewIndex, |_, _, _: (Value, Value)| {
            Err::<(), _>(err("CellId is read-only"))
        });
    }
}
fn dec_cell(lua: &Lua, j: &Json) -> Result<Value> {
    let b = tag_body(j, "$cell_id")?;
    let a = b
        .get("location")
        .and_then(Json::as_array)
        .filter(|v| v.len() == 3)
        .ok_or_else(|| err("invalid CellId location"))?;
    let mut loc = [0.; 3];
    for i in 0..3 {
        loc[i] = f32v(
            a[i].as_f64()
                .ok_or_else(|| err("invalid CellId location"))?,
            "location",
        )?
    }
    Ok(Value::UserData(
        lua.create_userdata(CellId {
            is_nil: b
                .get("is_nil")
                .and_then(Json::as_bool)
                .ok_or_else(|| err("invalid CellId"))?,
            location: loc,
        })?,
    ))
}
fn enc_cell(u: &AnyUserData) -> Result<Option<Json>> {
    let Ok(v) = u.borrow::<CellId>() else {
        return Ok(None);
    };
    Ok(Some(
        json!({"$cell_id":{"is_nil":v.is_nil,"location":v.location,"terrain_part":null}}),
    ))
}

#[derive(Clone, Debug)]
struct PluginDrag {
    fields: [Vec<u8>; 5],
    hotspot: [f32; 2],
}
impl UserData for PluginDrag {
    fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
        f.add_meta_field("__type", "PluginDrag");
        for (i, name) in ["Sender", "MimeType", "Data", "MouseIcon", "DragIcon"]
            .iter()
            .enumerate()
        {
            f.add_field_method_get(*name, move |lua, v| {
                Ok(Value::String(lua.create_string(&v.fields[i])?))
            });
        }
        f.add_field_method_get("HotSpot", |lua, v| {
            lua.create_userdata(Vector2 {
                x: v.hotspot[0] as f64,
                y: v.hotspot[1] as f64,
            })
        });
    }
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_meta_method(MetaMethod::NewIndex, |_, _, (key, _): (String, Value)| {
            Err::<(), _>(err(format!("{key} member of PluginDrag is read-only")))
        });
    }
}
fn encode_bytes(b: &[u8]) -> Json {
    if let Ok(s) = std::str::from_utf8(b) {
        json!(s)
    } else {
        json!({"$bytes_hex":hex(b)})
    }
}
fn decode_bytes(j: &Json) -> Result<Vec<u8>> {
    if let Some(s) = j.as_str() {
        Ok(s.as_bytes().to_vec())
    } else {
        decode_hex(
            j.get("$bytes_hex")
                .and_then(Json::as_str)
                .ok_or_else(|| err("invalid encoded byte string"))?,
        )
    }
}
fn dec_plugin_drag(lua: &Lua, j: &Json) -> Result<Value> {
    let b = tag_body(j, "$plugin_drag")?;
    if b.as_object().map_or(0, |m| m.len()) != 6 {
        return Err(err("invalid PluginDrag fields"));
    }
    let names = ["sender", "mime_type", "data", "mouse_icon", "drag_icon"];
    let mut fields: [Vec<u8>; 5] = std::array::from_fn(|_| vec![]);
    for (i, k) in names.iter().enumerate() {
        fields[i] = decode_bytes(b.get(*k).ok_or_else(|| err("missing PluginDrag field"))?)?;
    }
    let spot = b["hot_spot"]
        .as_array()
        .filter(|a| a.len() == 2)
        .ok_or_else(|| err("invalid PluginDrag HotSpot"))?;
    let mut hotspot = [0.; 2];
    for i in 0..2 {
        hotspot[i] = json_float(&spot[i])? as f32
    }
    Ok(Value::UserData(
        lua.create_userdata(PluginDrag { fields, hotspot })?,
    ))
}
fn json_float(j: &Json) -> Result<f64> {
    if let Some(n) = j.as_f64() {
        Ok(n)
    } else {
        match j.get("$number").and_then(Json::as_str) {
            Some("nan") => Ok(f64::NAN),
            Some("inf") => Ok(f64::INFINITY),
            Some("-inf") => Ok(f64::NEG_INFINITY),
            _ => Err(err("number expected")),
        }
    }
}
fn num_json(n: f32) -> Json {
    if n.is_nan() {
        json!({"$number":"nan"})
    } else if n == f32::INFINITY {
        json!({"$number":"inf"})
    } else if n == f32::NEG_INFINITY {
        json!({"$number":"-inf"})
    } else {
        json!(n)
    }
}
fn enc_plugin_drag(u: &AnyUserData) -> Result<Option<Json>> {
    let Ok(v) = u.borrow::<PluginDrag>() else {
        return Ok(None);
    };
    Ok(Some(
        json!({"$plugin_drag":{"sender":encode_bytes(&v.fields[0]),"mime_type":encode_bytes(&v.fields[1]),"data":encode_bytes(&v.fields[2]),"mouse_icon":encode_bytes(&v.fields[3]),"drag_icon":encode_bytes(&v.fields[4]),"hot_spot":[num_json(v.hotspot[0]),num_json(v.hotspot[1])]}}),
    ))
}
fn install_plugin_drag(lua: &Lua) -> Result<()> {
    let t = lua.create_table()?;
    t.set(
        "new",
        lua.create_function(|lua, args: mlua::Variadic<Value>| {
            let mut fields: [Vec<u8>; 5] = std::array::from_fn(|_| vec![]);
            for (i, v) in args.iter().take(5).enumerate() {
                let Value::String(s) = v else {
                    return Err(err(format!(
                        "PluginDrag.new argument {} must be a string/ContentId",
                        i + 1
                    )));
                };
                fields[i] = s.as_bytes().to_vec()
            }
            let hotspot = if args.len() >= 6 {
                let Value::UserData(d) = &args[5] else {
                    return Err(err(
                        "PluginDrag.new sixth arg should be a Vector2 for HotSpot",
                    ));
                };
                let v = d
                    .borrow::<Vector2>()
                    .map_err(|_| err("PluginDrag.new sixth arg should be a Vector2 for HotSpot"))?;
                [v.x as f32, v.y as f32]
            } else {
                [0., 0.]
            };
            lua.create_userdata(PluginDrag { fields, hotspot })
        })?,
    )?;
    lua.globals().set("PluginDrag", t)?;
    values::register_codec(
        lua,
        Codec {
            key: "$plugin_drag",
            type_name: "PluginDrag",
            decode: dec_plugin_drag,
            encode: enc_plugin_drag,
        },
    )
}

#[derive(Clone, Debug)]
struct Content {
    kind: i32,
    uri: Vec<u8>,
    object: Option<String>,
    world: Weak<RefCell<World>>,
}
impl UserData for Content {
    fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
        f.add_meta_field("__type", "Content");
        f.add_field_method_get("SourceType", |lua, v| {
            enum_value(lua, "ContentSourceType", v.kind)
        });
        f.add_field_method_get("Uri", |lua, v| {
            if v.kind == 1 {
                Ok(Value::String(lua.create_string(&v.uri)?))
            } else {
                Ok(Value::Nil)
            }
        });
        f.add_field_method_get("Object", |lua, v| {
            if v.kind == 2 {
                if let (Some(w), Some(id)) = (v.world.upgrade(), v.object.as_ref()) {
                    world::instance(&lua, &w, id)
                } else {
                    Ok(Value::Nil)
                }
            } else {
                Ok(Value::Nil)
            }
        });
        f.add_field_method_get("Opaque", |_, _| Ok(Value::Nil));
    }
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_meta_method(MetaMethod::NewIndex, |_, _, _: (Value, Value)| {
            Err::<(), _>(err("Content values are immutable"))
        });
        m.add_meta_method(MetaMethod::Eq, |_, a, b: AnyUserData| {
            Ok(b.borrow::<Self>()
                .map(|b| {
                    a.kind == b.kind
                        && a.uri == b.uri
                        && a.object == b.object
                        && (a.kind != 2 || Weak::ptr_eq(&a.world, &b.world))
                })
                .unwrap_or(false))
        });
    }
}
fn decode_hex(s: &str) -> Result<Vec<u8>> {
    if s.len() % 2 != 0 || s.len() > 2 * 1024 * 1024 {
        return Err(err("invalid hex bytes"));
    }
    let mut b = Vec::with_capacity(s.len() / 2);
    for p in s.as_bytes().chunks_exact(2) {
        let h = (p[0] as char)
            .to_digit(16)
            .ok_or_else(|| err("invalid hex bytes"))?;
        let l = (p[1] as char)
            .to_digit(16)
            .ok_or_else(|| err("invalid hex bytes"))?;
        b.push(((h << 4) | l) as u8)
    }
    Ok(b)
}
fn hex(b: &[u8]) -> String {
    const D: &[u8] = b"0123456789abcdef";
    let mut s = String::with_capacity(2 * b.len());
    for x in b {
        s.push(D[(x >> 4) as usize] as char);
        s.push(D[(x & 15) as usize] as char)
    }
    s
}
fn dec_content(lua: &Lua, j: &Json, ctx: &WorldRef) -> Result<Value> {
    let b = tag_body(j, "$content")?;
    let k = b
        .get("source_type")
        .and_then(Json::as_i64)
        .ok_or_else(|| err("invalid Content source"))? as i32;
    let v = match k {
        0 if b.as_object().is_some_and(|m| m.len() == 1) => Content {
            kind: 0,
            uri: vec![],
            object: None,
            world: Rc::downgrade(ctx),
        },
        1 if b.as_object().is_some_and(|m| m.len() == 2) => {
            let uri = decode_hex(
                b.get("uri_hex")
                    .and_then(Json::as_str)
                    .ok_or_else(|| err("missing Content uri"))?,
            )?;
            if uri.is_empty() {
                return Err(err("invalid empty Content URI"));
            }
            Content {
                kind: 1,
                uri,
                object: None,
                world: Rc::downgrade(ctx),
            }
        }
        2 if b.as_object().is_some_and(|m| m.len() == 2) => {
            let id = b
                .get("object_id")
                .and_then(Json::as_str)
                .ok_or_else(|| err("missing Content object id"))?;
            if !ctx.borrow().nodes.contains_key(id) {
                return Err(err("Content object is unavailable"));
            }
            Content {
                kind: 2,
                uri: vec![],
                object: Some(id.into()),
                world: Rc::downgrade(ctx),
            }
        }
        _ => return Err(err("invalid Content value")),
    };
    Ok(Value::UserData(lua.create_userdata(v)?))
}
fn enc_content(u: &AnyUserData) -> Result<Option<Json>> {
    let Ok(v) = u.borrow::<Content>() else {
        return Ok(None);
    };
    let mut b = json!({"source_type":v.kind});
    if v.kind == 1 {
        b["uri_hex"] = json!(hex(&v.uri))
    }
    if v.kind == 2 {
        b["object_id"] = json!(v.object)
    }
    Ok(Some(json!({"$content":b})))
}

#[derive(Clone, Debug)]
struct Catalog(Json);
const CAT_FIELDS: &[(&str, &str)] = &[
    ("SearchKeyword", "str"),
    ("MinPrice", "num"),
    ("MaxPrice", "num"),
    ("SortType", "CatalogSortType"),
    ("SortAggregation", "CatalogSortAggregation"),
    ("CategoryFilter", "CatalogCategoryFilter"),
    ("SalesTypeFilter", "SalesTypeFilter"),
    ("BundleTypes", "BundleType[]"),
    ("AssetTypes", "AvatarAssetType[]"),
    ("IncludeOffSale", "bool"),
    ("CreatorName", "str"),
    ("CreatorType", "CreatorTypeFilter"),
    ("CreatorId", "num"),
    ("Limit", "num"),
];
fn catalog_default() -> Json {
    json!({"SearchKeyword":{"hex":""},"MinPrice":0,"MaxPrice":2147483647,"SortType":1,"SortAggregation":6,"CategoryFilter":1,"SalesTypeFilter":1,"BundleTypes":[],"AssetTypes":[],"IncludeOffSale":false,"CreatorName":{"hex":""},"CreatorType":2,"CreatorId":0,"Limit":30})
}
impl UserData for Catalog {
    fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
        f.add_meta_field("__type", "CatalogSearchParams");
        for &(name, kind) in CAT_FIELDS {
            match kind {
                "str" => f.add_field_method_get(name, move |lua, v| {
                    let h = v.0[name].get("hex").and_then(Json::as_str).unwrap_or("");
                    let bytes =
                        decode_hex(h).map_err(|e| mlua::Error::RuntimeError(e.to_string()))?;
                    Ok(Value::String(lua.create_string(bytes)?))
                }),
                "bool" => f.add_field_method_get(name, move |_, v| {
                    Ok(v.0[name].as_bool().unwrap_or(false))
                }),
                k if k.ends_with("[]") => f.add_field_method_get(name, move |lua, v| {
                    let ty = &k[..k.len() - 2];
                    let a = v.0[name]
                        .as_array()
                        .ok_or_else(|| err("invalid enum list"))?;
                    let t = lua.create_table()?;
                    for (i, n) in a.iter().enumerate() {
                        t.set(
                            i + 1,
                            enum_value(
                                lua,
                                ty,
                                n.as_i64().ok_or_else(|| err("enum integer expected"))? as i32,
                            )?,
                        )?
                    }
                    t.set_readonly(true);
                    Ok(t)
                }),
                k if k != "num" => f.add_field_method_get(name, move |lua, v| {
                    enum_value(lua, k, v.0[name].as_i64().unwrap_or(0) as i32)
                }),
                _ => {
                    f.add_field_method_get(name, move |_, v| Ok(v.0[name].as_f64().unwrap_or(0.0)))
                }
            }
        }
    }
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_meta_method_mut(
            MetaMethod::NewIndex,
            |lua, v, (key, value): (String, Value)| {
                let kind = CAT_FIELDS
                    .iter()
                    .find(|x| x.0 == key)
                    .map(|x| x.1)
                    .ok_or_else(|| err("unknown CatalogSearchParams member"))?;
                let mut data = v.0.clone();
                match kind {
                    "str" => {
                        let Value::String(text) = value else {
                            return Err(err("string expected"));
                        };
                        let b = text.as_bytes();
                        if b.len() > 1024 * 1024 {
                            return Err(err("catalog string exceeds host limit"));
                        }
                        data[&key] = json!({"hex":hex(&b)})
                    }
                    "bool" => {
                        if let Value::Boolean(x) = value {
                            data[&key] = json!(x)
                        } else {
                            return Err(err("boolean expected"));
                        }
                    }
                    k if k.ends_with("[]") => {
                        let t = match value {
                            Value::Table(t) => t,
                            _ => return Err(err("enum array expected")),
                        };
                        let ty = &k[..k.len() - 2];
                        let mut a = Vec::new();
                        for pair in t.pairs::<i64, Value>() {
                            let (i, x) = pair?;
                            if i <= 0 || i as usize > 4096 {
                                return Err(err("dense enum array expected"));
                            }
                            a.push((i, read_enum(&lua, x, ty)?))
                        }
                        a.sort_by_key(|x| x.0);
                        if a.iter().enumerate().any(|(i, x)| x.0 as usize != i + 1) {
                            return Err(err("dense enum array expected"));
                        }
                        data[&key] = json!(a.into_iter().map(|x| x.1).collect::<Vec<_>>())
                    }
                    k if k != "num" => data[&key] = json!(read_enum(&lua, value, k)?),
                    _ => {
                        let n = match value {
                            Value::Number(n) => n,
                            Value::Integer(n) => n as f64,
                            _ => return Err(err("number expected")),
                        };
                        if !n.is_finite() {
                            return Err(err("finite number expected"));
                        }
                        if key == "Limit" && ![10., 28., 30., 60., 120.].contains(&n) {
                            return Err(err("Limit must be 10, 28, 30, 60, or 120"));
                        }
                        data[&key] = json!(n)
                    }
                }
                *v = Self(data);
                Ok(())
            },
        );
    }
}
fn dec_catalog(lua: &Lua, j: &Json) -> Result<Value> {
    let b = tag_body(j, "$catalog_search_params")?;
    if b.as_object().map_or(0, |m| m.len()) != CAT_FIELDS.len() {
        return Err(err("invalid CatalogSearchParams fields"));
    }
    for (k, _) in CAT_FIELDS {
        if b.get(*k).is_none() {
            return Err(err("missing CatalogSearchParams field"));
        }
    }
    let mut data = b.clone();
    for key in ["SearchKeyword", "CreatorName"] {
        let x = b[key]
            .get("hex")
            .and_then(Json::as_str)
            .ok_or_else(|| err("invalid catalog string"))?;
        let _ = decode_hex(x)?;
    }
    for &(key, kind) in CAT_FIELDS {
        let item = &b[key];
        match kind {
            "str" => {
                let h = item
                    .get("hex")
                    .and_then(Json::as_str)
                    .ok_or_else(|| err("invalid catalog string"))?;
                let _ = decode_hex(h)?;
            }
            "bool" => {
                if !item.is_boolean() {
                    return Err(err("invalid catalog boolean"));
                }
            }
            "num" => {
                let n = item.as_f64().ok_or_else(|| err("invalid catalog number"))?;
                if !n.is_finite() || (key == "Limit" && ![10., 28., 30., 60., 120.].contains(&n)) {
                    return Err(err("invalid catalog number"));
                }
            }
            k if k.ends_with("[]") => {
                let a = item
                    .as_array()
                    .filter(|a| a.len() <= 4096)
                    .ok_or_else(|| err("invalid catalog enum array"))?;
                for number in a {
                    let n = number
                        .as_i64()
                        .ok_or_else(|| err("invalid catalog enum value"))?
                        as i32;
                    let _ = enum_value(lua, &k[..k.len() - 2], n)?;
                }
            }
            enum_type => {
                let n = item
                    .as_i64()
                    .ok_or_else(|| err("invalid catalog enum value"))?
                    as i32;
                let _ = enum_value(lua, enum_type, n)?;
            }
        }
    }
    Ok(Value::UserData(lua.create_userdata(Catalog(data))?))
}
fn enc_catalog(u: &AnyUserData) -> Result<Option<Json>> {
    let Ok(v) = u.borrow::<Catalog>() else {
        return Ok(None);
    };
    Ok(Some(json!({"$catalog_search_params":v.0})))
}

#[derive(Clone, Debug)]
struct QueryParams {
    world: Weak<RefCell<World>>,
    filter_type: i32,
    legacy: Vec<String>,
    include: Option<Vec<String>>,
    exclude: Option<Vec<String>>,
    respect: bool,
    brute: bool,
    group: String,
    ignore_water: bool,
    margin: f64,
    size_gradient: f64,
    max_parts: u32,
    tolerance: f64,
}
#[derive(Clone, Debug)]
struct RaycastParamsValue(QueryParams);
#[derive(Clone, Debug)]
struct OverlapParamsValue(QueryParams);
impl std::ops::Deref for RaycastParamsValue {
    type Target = QueryParams;
    fn deref(&self) -> &QueryParams {
        &self.0
    }
}
impl std::ops::DerefMut for RaycastParamsValue {
    fn deref_mut(&mut self) -> &mut QueryParams {
        &mut self.0
    }
}
impl std::ops::Deref for OverlapParamsValue {
    type Target = QueryParams;
    fn deref(&self) -> &QueryParams {
        &self.0
    }
}
impl std::ops::DerefMut for OverlapParamsValue {
    fn deref_mut(&mut self) -> &mut QueryParams {
        &mut self.0
    }
}
#[derive(Clone, Debug)]
pub struct QueryConfig {
    pub overlap: bool,
    pub filter_type: i32,
    pub legacy: Vec<String>,
    pub include: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
    pub respect: bool,
    pub brute: bool,
    pub group: String,
    pub ignore_water: bool,
    pub margin: f64,
    pub size_gradient: f64,
    pub max_parts: u32,
    pub tolerance: f64,
}
pub fn query_config(value: &Value) -> Option<QueryConfig> {
    let Value::UserData(u) = value else {
        return None;
    };
    let (p, overlap) = if let Ok(v) = u.borrow::<RaycastParamsValue>() {
        (v.0.clone(), false)
    } else if let Ok(v) = u.borrow::<OverlapParamsValue>() {
        (v.0.clone(), true)
    } else {
        return None;
    };
    Some(QueryConfig {
        overlap,
        filter_type: p.filter_type,
        legacy: p.legacy.clone(),
        include: p.include.clone(),
        exclude: p.exclude.clone(),
        respect: p.respect,
        brute: p.brute,
        group: p.group.clone(),
        ignore_water: p.ignore_water,
        margin: p.margin,
        size_gradient: p.size_gradient,
        max_parts: p.max_parts,
        tolerance: p.tolerance,
    })
}
pub fn query_params_belongs_to(value: &Value, world: &WorldRef) -> bool {
    let Value::UserData(u) = value else {
        return false;
    };
    u.borrow::<RaycastParamsValue>()
        .map(|p| Weak::ptr_eq(&p.world, &Rc::downgrade(world)))
        .or_else(|_| {
            u.borrow::<OverlapParamsValue>()
                .map(|p| Weak::ptr_eq(&p.world, &Rc::downgrade(world)))
        })
        .unwrap_or(false)
}

macro_rules! impl_query_userdata {
    ($wrapper:ident, $type_name:literal) => {
        impl UserData for $wrapper {
            fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
                f.add_meta_field("__type", $type_name);
                macro_rules! setbool {
                    ($n:literal,$field:ident) => {
                        f.add_field_method_get($n, |_, v| Ok(v.$field));
                        f.add_field_method_set($n, |_, v, x: bool| {
                            v.$field = x;
                            Ok(())
                        });
                    };
                }
                setbool!("RespectCanCollide", respect);
                setbool!("BruteForceAllSlow", brute);
                setbool!("IgnoreWater", ignore_water);
                f.add_field_method_get("CollisionGroup", |_, v| Ok(v.group.clone()));
                f.add_field_method_set("CollisionGroup", |_, v, s: String| {
                    if s.is_empty() || s.len() > 100 || s.as_bytes().contains(&0) {
                        return Err(err("invalid CollisionGroup name"));
                    }
                    v.group = s;
                    Ok(())
                });
                f.add_field_method_get("Margin", |_, v| Ok(v.margin));
                f.add_field_method_set("Margin", |_, v, x: f64| {
                    if !x.is_finite() || x < 0.0 {
                        return Err(err("invalid Margin"));
                    }
                    v.margin = x;
                    Ok(())
                });
                f.add_field_method_get("SizeGradient", |_, v| Ok(v.size_gradient));
                f.add_field_method_set("SizeGradient", |_, v, x: f64| {
                    if !x.is_finite() || x < -1.0 {
                        return Err(err("invalid SizeGradient"));
                    }
                    v.size_gradient = x;
                    Ok(())
                });
                f.add_field_method_get("MaxParts", |_, v| Ok(v.max_parts));
                f.add_field_method_set("MaxParts", |_, v, x: Value| {
                    let n = match x {
                        Value::Integer(n) => n,
                        Value::Number(n) if n.fract() == 0.0 => n as i64,
                        _ => return Err(err("MaxParts requires an integer from 0 to 2147483647")),
                    };
                    if !(0..=i32::MAX as i64).contains(&n) {
                        return Err(err("MaxParts requires an integer from 0 to 2147483647"));
                    }
                    v.max_parts = n as u32;
                    Ok(())
                });
                f.add_field_method_get("Tolerance", |_, v| Ok(v.tolerance));
                f.add_field_method_set("Tolerance", |_, v, x: f64| {
                    if !x.is_finite() {
                        return Err(err("invalid Tolerance"));
                    }
                    v.tolerance = x.clamp(0., 0.05);
                    Ok(())
                });
                f.add_field_method_get("FilterType", |lua, v| {
                    enum_value(lua, "RaycastFilterType", v.filter_type)
                });
                f.add_field_method_set("FilterType", |lua, v, x: Value| {
                    v.filter_type = read_enum(&lua, x, "RaycastFilterType")?;
                    Ok(())
                });
                f.add_field_method_get("FilterDescendantsInstances", |lua, v| {
                    let w = v.world.upgrade().ok_or_else(|| err("world released"))?;
                    id_array(&lua, &w, &v.legacy)
                });
                f.add_field_method_set("FilterDescendantsInstances", |_, v, x: Value| {
                    v.legacy = parse_ids(x, &v.world)?;
                    Ok(())
                });
                f.add_field_method_get("IncludeInstances", |lua, v| {
                    if let Some(a) = &v.include {
                        id_array(
                            &lua,
                            &v.world.upgrade().ok_or_else(|| err("world released"))?,
                            a,
                        )
                    } else {
                        Ok(Value::Nil)
                    }
                });
                f.add_field_method_set("IncludeInstances", |_, v, x: Value| {
                    v.include = if matches!(x, Value::Nil) {
                        None
                    } else {
                        Some(parse_ids(x, &v.world)?)
                    };
                    Ok(())
                });
                f.add_field_method_get("ExcludeInstances", |lua, v| {
                    if let Some(a) = &v.exclude {
                        id_array(
                            &lua,
                            &v.world.upgrade().ok_or_else(|| err("world released"))?,
                            a,
                        )
                    } else {
                        Ok(Value::Nil)
                    }
                });
                f.add_field_method_set("ExcludeInstances", |_, v, x: Value| {
                    v.exclude = if matches!(x, Value::Nil) {
                        None
                    } else {
                        Some(parse_ids(x, &v.world)?)
                    };
                    Ok(())
                });
            }
            fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
                m.add_method_mut("AddToFilter", |_, v, x: Value| {
                    let ids = parse_ids_single_or_table(x, &v.world)?;
                    if v.legacy.len() + ids.len() > 4096 {
                        return Err(err("spatial query filter limit exceeded"));
                    }
                    v.legacy.extend(ids);
                    Ok(())
                });
            }
        }
    };
}
impl_query_userdata!(RaycastParamsValue, "RaycastParams");
impl_query_userdata!(OverlapParamsValue, "OverlapParams");

fn parse_ids(v: Value, w: &Weak<RefCell<World>>) -> Result<Vec<String>> {
    let t = match v {
        Value::Table(t) => t,
        _ => return Err(err("filter requires an array of Instances")),
    };
    let mut ids = Vec::new();
    for p in t.pairs::<i64, Value>() {
        let (i, x) = p?;
        if i <= 0 || i > 4096 {
            return Err(err("filter requires dense array"));
        }
        let owner = w.upgrade().ok_or_else(|| err("world released"))?;
        if !world::instance_belongs_to(&x, &owner) {
            return Err(err("filter Instance belongs to another world"));
        }
        ids.push((
            i,
            world::instance_id(&x).ok_or_else(|| err("filter requires Instances"))?,
        ))
    }
    ids.sort_by_key(|x| x.0);
    if ids.iter().enumerate().any(|(i, x)| x.0 as usize != i + 1) {
        return Err(err("filter requires dense array"));
    }
    validate_world_ids(&ids.iter().map(|x| x.1.clone()).collect::<Vec<_>>(), w)?;
    Ok(ids.into_iter().map(|x| x.1).collect())
}
fn parse_ids_single_or_table(v: Value, w: &Weak<RefCell<World>>) -> Result<Vec<String>> {
    if let Some(id) = world::instance_id(&v) {
        let owner = w.upgrade().ok_or_else(|| err("world released"))?;
        if !world::instance_belongs_to(&v, &owner) {
            return Err(err("filter Instance belongs to another world"));
        }
        validate_world_ids(std::slice::from_ref(&id), w)?;
        Ok(vec![id])
    } else {
        parse_ids(v, w)
    }
}
fn validate_world_ids(ids: &[String], w: &Weak<RefCell<World>>) -> Result<()> {
    let world = w.upgrade().ok_or_else(|| err("world released"))?;
    for id in ids {
        if !world.borrow().nodes.contains_key(id) {
            return Err(err("filter Instance belongs to another world"));
        }
    }
    Ok(())
}
fn id_array(lua: &Lua, w: &WorldRef, ids: &[String]) -> Result<Value> {
    let t = lua.create_table()?;
    for (i, id) in ids.iter().enumerate() {
        t.set(i + 1, world::instance(lua, w, id)?)?
    }
    Ok(Value::Table(t))
}
fn install_query_params(lua: &Lua, ctx: &WorldRef) -> Result<()> {
    let c = Rc::downgrade(ctx);
    for (name, is_overlap) in [("RaycastParams", false), ("OverlapParams", true)] {
        let world_weak = c.clone();
        let t = lua.create_table()?;
        t.set(
            "new",
            lua.create_function(move |lua, ()| {
                let data = QueryParams {
                    world: world_weak.clone(),
                    filter_type: 0,
                    legacy: vec![],
                    include: None,
                    exclude: None,
                    respect: false,
                    brute: false,
                    group: "Default".into(),
                    ignore_water: false,
                    margin: 0.,
                    size_gradient: 0.,
                    max_parts: 0,
                    tolerance: 0.,
                };
                if is_overlap {
                    lua.create_userdata(OverlapParamsValue(data))
                } else {
                    lua.create_userdata(RaycastParamsValue(data))
                }
            })?,
        )?;
        lua.globals().set(name, t)?;
        if is_overlap {
            values::register_codec(
                lua,
                Codec {
                    key: "$overlap_params",
                    type_name: "OverlapParams",
                    decode: dec_query,
                    encode: enc_query,
                },
            )?
        } else {
            values::register_codec(
                lua,
                Codec {
                    key: "$raycast_params",
                    type_name: "RaycastParams",
                    decode: dec_query,
                    encode: enc_query,
                },
            )?
        }
    }
    Ok(())
}
fn enc_query(u: &AnyUserData) -> Result<Option<Json>> {
    let (p, is_overlap) = if let Ok(v) = u.borrow::<OverlapParamsValue>() {
        (v.0.clone(), true)
    } else if let Ok(v) = u.borrow::<RaycastParamsValue>() {
        (v.0.clone(), false)
    } else {
        return Ok(None);
    };
    let body = json!({"filter_type":p.filter_type,"legacy":p.legacy,"include":p.include,"exclude":p.exclude,"respect_can_collide":p.respect,"brute_force":p.brute,"collision_group":p.group,"ignore_water":p.ignore_water,"margin":p.margin,"size_gradient":p.size_gradient,"max_parts":p.max_parts,"tolerance":p.tolerance});
    Ok(Some(if is_overlap {
        json!({"$overlap_params":body})
    } else {
        json!({"$raycast_params":body})
    }))
}
fn dec_query(lua: &Lua, j: &Json) -> Result<Value> {
    let b = if j.get("$overlap_params").is_some() {
        tag_body(j, "$overlap_params")?
    } else {
        tag_body(j, "$raycast_params")?
    };
    let parse = |k: &str| b.get(k).ok_or_else(|| err(format!("missing {k}")));
    let list = |k: &str| -> Result<Vec<String>> {
        Ok(parse(k)?
            .as_array()
            .ok_or_else(|| err("invalid filter"))?
            .iter()
            .map(|x| {
                x.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| err("invalid filter id"))
            })
            .collect::<Result<Vec<_>>>()?)
    };
    let legacy = list("legacy")?;
    let include = if parse("include")?.is_null() {
        None
    } else {
        Some(list("include")?)
    };
    let exclude = if parse("exclude")?.is_null() {
        None
    } else {
        Some(list("exclude")?)
    };
    let context = lua
        .app_data_ref::<WorldRef>()
        .map(|w| w.clone())
        .ok_or_else(|| err("spatial query params require a World"))?;
    let weak = Rc::downgrade(&context);
    let all_ids = legacy
        .iter()
        .chain(include.iter().flatten())
        .chain(exclude.iter().flatten())
        .cloned()
        .collect::<Vec<_>>();
    validate_world_ids(&all_ids, &weak)?;
    let p = QueryParams {
        world: weak,
        filter_type: parse("filter_type")?.as_i64().unwrap_or(0) as i32,
        legacy,
        include,
        exclude,
        respect: parse("respect_can_collide")?.as_bool().unwrap_or(false),
        brute: parse("brute_force")?.as_bool().unwrap_or(false),
        group: parse("collision_group")?
            .as_str()
            .unwrap_or("Default")
            .into(),
        ignore_water: parse("ignore_water")?.as_bool().unwrap_or(false),
        margin: parse("margin")?.as_f64().unwrap_or(0.),
        size_gradient: parse("size_gradient")?.as_f64().unwrap_or(0.),
        max_parts: parse("max_parts")?
            .as_u64()
            .unwrap_or(0)
            .min(u32::MAX as u64) as u32,
        tolerance: parse("tolerance")?.as_f64().unwrap_or(0.),
    };
    if j.get("$overlap_params").is_some() {
        Ok(Value::UserData(lua.create_userdata(OverlapParamsValue(p))?))
    } else {
        Ok(Value::UserData(lua.create_userdata(RaycastParamsValue(p))?))
    }
}

#[derive(Clone, Debug)]
struct DockInfo {
    enabled: bool,
    state: i32,
    override_enabled: bool,
    width: f64,
    height: f64,
    min_width: f64,
    min_height: f64,
}
impl UserData for DockInfo {
    fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
        f.add_meta_field("__type", "DockWidgetPluginGuiInfo");
        f.add_field_method_get("InitialEnabled", |_, v| Ok(v.enabled));
        f.add_field_method_get("InitialDockState", |lua, v| {
            enum_value(lua, "InitialDockState", v.state)
        });
        f.add_field_method_get("OverrideEnabled", |_, v| Ok(v.override_enabled));
        f.add_field_method_get("FloatingXSize", |_, v| Ok(v.width));
        f.add_field_method_get("FloatingYSize", |_, v| Ok(v.height));
        f.add_field_method_get("MinWidth", |_, v| Ok(v.min_width));
        f.add_field_method_get("MinHeight", |_, v| Ok(v.min_height));
    }
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_meta_method(MetaMethod::NewIndex, |_, _, _: (Value, Value)| {
            Err::<(), _>(err("DockWidgetPluginGuiInfo is immutable"))
        });
    }
}
fn dec_dock(lua: &Lua, j: &Json) -> Result<Value> {
    let b = tag_body(j, "$dock_widget_plugin_gui_info")?;
    let p = DockInfo {
        enabled: b["enabled"]
            .as_bool()
            .ok_or_else(|| err("invalid enabled"))?,
        state: b["state"].as_i64().ok_or_else(|| err("invalid state"))? as i32,
        override_enabled: b["override_enabled"]
            .as_bool()
            .ok_or_else(|| err("invalid override"))?,
        width: b["width"].as_f64().ok_or_else(|| err("invalid width"))?,
        height: b["height"].as_f64().ok_or_else(|| err("invalid height"))?,
        min_width: b["min_width"]
            .as_f64()
            .ok_or_else(|| err("invalid min width"))?,
        min_height: b["min_height"]
            .as_f64()
            .ok_or_else(|| err("invalid min height"))?,
    };
    Ok(Value::UserData(lua.create_userdata(p)?))
}
fn enc_dock(u: &AnyUserData) -> Result<Option<Json>> {
    let Ok(v) = u.borrow::<DockInfo>() else {
        return Ok(None);
    };
    Ok(Some(
        json!({"$dock_widget_plugin_gui_info":{"enabled":v.enabled,"state":v.state,"override_enabled":v.override_enabled,"width":v.width,"height":v.height,"min_width":v.min_width,"min_height":v.min_height}}),
    ))
}

fn install_font(lua: &Lua) -> Result<()> {
    let t = lua.create_table()?;
    t.set(
        "new",
        lua.create_function(
            |lua, (family, weight, style): (String, Option<Value>, Option<Value>)| {
                if family.len() > 4096 || family.as_bytes().contains(&0) {
                    return Err(err("invalid Font family"));
                }
                let w = if let Some(x) = weight {
                    read_enum(lua, x, "FontWeight")?
                } else {
                    400
                };
                let s = if let Some(x) = style {
                    read_enum(lua, x, "FontStyle")?
                } else {
                    0
                };
                if !(100..=900).contains(&w) || w % 100 != 0 || !(0..=1).contains(&s) {
                    return Err(err("invalid Font components"));
                }
                lua.create_userdata(Font {
                    family,
                    weight: w,
                    style: s,
                })
            },
        )?,
    )?;
    t.set(
        "fromName",
        lua.create_function(
            |lua, (name, weight, style): (String, Option<Value>, Option<Value>)| {
                if name.is_empty()
                    || name.len() > 4000
                    || !name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                {
                    return Err(err("invalid font name"));
                }
                let w = if let Some(x) = weight {
                    read_enum(lua, x, "FontWeight")?
                } else {
                    400
                };
                let s = if let Some(x) = style {
                    read_enum(lua, x, "FontStyle")?
                } else {
                    0
                };
                lua.create_userdata(Font {
                    family: format!("rbxasset://fonts/families/{name}.json"),
                    weight: w,
                    style: s,
                })
            },
        )?,
    )?;
    t.set(
        "fromId",
        lua.create_function(
            |lua, (id, weight, style): (f64, Option<Value>, Option<Value>)| {
                if !id.is_finite() || id < 0. || id.fract() != 0. {
                    return Err(err("font asset ID must be a nonnegative integer"));
                }
                let w = if let Some(x) = weight {
                    read_enum(lua, x, "FontWeight")?
                } else {
                    400
                };
                let s = if let Some(x) = style {
                    read_enum(lua, x, "FontStyle")?
                } else {
                    0
                };
                lua.create_userdata(Font {
                    family: format!("rbxassetid://{id:.0}"),
                    weight: w,
                    style: s,
                })
            },
        )?,
    )?;
    t.set(
        "fromEnum",
        lua.create_function(|lua, item: Value| {
            let n = read_enum(lua, item, "Font")?;
            let (name, weight, style) = *LEGACY_FONTS
                .get(n as usize)
                .ok_or_else(|| err("Enum.Font.Unknown cannot construct a Font"))?;
            lua.create_userdata(Font {
                family: format!("rbxasset://fonts/families/{name}.json"),
                weight,
                style,
            })
        })?,
    )?;
    lua.globals().set("Font", t)
}
fn install_directions(lua: &Lua) -> Result<()> {
    for (name, is_axes) in [("Axes", true), ("Faces", false)] {
        let ctor = lua.create_table()?;
        ctor.set(
            "new",
            lua.create_function(move |lua, args: mlua::Variadic<Value>| {
                if is_axes && args.len() > 6 {
                    return Err(err("Axes.new accepts up to six Axis or NormalId items"));
                }
                let mut bits = 0u8;
                for value in args {
                    let j = values::to_json(lua, &value)?;
                    let Some(e) = j.get("$enum") else {
                        if is_axes {
                            return Err(err("Enum.Axis or Enum.NormalId expected"));
                        }
                        continue;
                    };
                    let ty = e.get("type").and_then(Json::as_str).unwrap_or("");
                    let n = e.get("value").and_then(Json::as_i64).unwrap_or(-1);
                    if ty == "Axis" && is_axes && (0..3).contains(&n) {
                        bits |= 1 << n
                    } else if ty == "NormalId" && (0..6).contains(&n) {
                        bits |= 1 << (if is_axes { (n % 3) as u8 } else { n as u8 })
                    } else if is_axes {
                        return Err(err("Enum.Axis or Enum.NormalId expected"));
                    }
                }
                if is_axes {
                    lua.create_userdata(AxesValue(DirectionSet { bits, axes: true }))
                } else {
                    lua.create_userdata(FacesValue(DirectionSet { bits, axes: false }))
                }
            })?,
        )?;
        lua.globals().set(name, ctor)?
    }
    Ok(())
}
fn install_cell(lua: &Lua) -> Result<()> {
    let t = lua.create_table()?;
    t.set(
        "new",
        lua.create_function(
            |lua, (nil, x, y, z): (Option<bool>, Option<f64>, Option<f64>, Option<f64>)| {
                let l = [
                    f32v(x.unwrap_or(0.), "x")?,
                    f32v(y.unwrap_or(0.), "y")?,
                    f32v(z.unwrap_or(0.), "z")?,
                ];
                lua.create_userdata(CellId {
                    is_nil: nil.unwrap_or(false),
                    location: l,
                })
            },
        )?,
    )?;
    lua.globals().set("CellId", t)
}
fn install_content(lua: &Lua, ctx: &WorldRef) -> Result<()> {
    let t = lua.create_table()?;
    t.set(
        "none",
        lua.create_userdata(Content {
            kind: 0,
            uri: vec![],
            object: None,
            world: Rc::downgrade(ctx),
        })?,
    )?;
    t.set(
        "fromUri",
        lua.create_function(|lua, s: mlua::String| {
            let b = s.as_bytes().to_vec();
            if b.len() > 1024 * 1024 {
                return Err(err("Content URI exceeds limit"));
            }
            lua.create_userdata(Content {
                kind: if b.is_empty() { 0 } else { 1 },
                uri: b,
                object: None,
                world: Weak::new(),
            })
        })?,
    )?;
    let c = Rc::downgrade(ctx);
    t.set(
        "fromAssetId",
        lua.create_function(move |lua, id: f64| {
            if !id.is_finite() {
                return Err(err("finite asset ID expected"));
            }
            let uri = if id == 0. {
                vec![]
            } else {
                format!("rbxassetid://{id}").into_bytes()
            };
            lua.create_userdata(Content {
                kind: if id == 0. { 0 } else { 1 },
                uri,
                object: None,
                world: c.clone(),
            })
        })?,
    )?;
    let c = Rc::downgrade(ctx);
    t.set(
        "fromObject",
        lua.create_function(move |lua, v: Value| {
            let w = c.upgrade().ok_or_else(|| err("world released"))?;
            if !world::instance_belongs_to(&v, &w) {
                return Err(err("Instance belongs to another world"));
            }
            let id = world::instance_id(&v).ok_or_else(|| err("Instance expected"))?;
            if !w.borrow().nodes.contains_key(&id) {
                return Err(err("Instance belongs to another world"));
            }
            lua.create_userdata(Content {
                kind: 2,
                uri: vec![],
                object: Some(id),
                world: c.clone(),
            })
        })?,
    )?;
    lua.globals().set("Content", t)?;
    values::register_codec(
        lua,
        Codec {
            key: "$content",
            type_name: "Content",
            decode: |lua, j| {
                let w = lua
                    .app_data_ref::<WorldRef>()
                    .ok_or_else(|| err("Content requires world context"))?;
                dec_content(lua, j, &w)
            },
            encode: enc_content,
        },
    )
}
fn install_catalog(lua: &Lua) -> Result<()> {
    let t = lua.create_table()?;
    t.set(
        "new",
        lua.create_function(|lua, ()| lua.create_userdata(Catalog(catalog_default())))?,
    )?;
    lua.globals().set("CatalogSearchParams", t)?;
    values::register_codec(
        lua,
        Codec {
            key: "$catalog_search_params",
            type_name: "CatalogSearchParams",
            decode: dec_catalog,
            encode: enc_catalog,
        },
    )?;
    values::register_codec(
        lua,
        Codec {
            key: "$dock_widget_plugin_gui_info",
            type_name: "DockWidgetPluginGuiInfo",
            decode: dec_dock,
            encode: enc_dock,
        },
    )?;
    let t = lua.create_table()?;
    t.set(
        "new",
        lua.create_function(
            |lua,
             (state, enabled, over, width, height, min_width, min_height): (
                Value,
                Value,
                Value,
                f64,
                f64,
                f64,
                f64,
            )| {
                let state = read_enum(lua, state, "InitialDockState")?;
                let Value::Boolean(enabled) = enabled else {
                    return Err(err("InitialEnabled must be a boolean"));
                };
                let Value::Boolean(over) = over else {
                    return Err(err("InitialEnabledShouldOverrideRestore must be a boolean"));
                };
                for n in [width, height, min_width, min_height] {
                    if !n.is_finite() {
                        return Err(err("DockWidget sizes must be finite"));
                    }
                }
                lua.create_userdata(DockInfo {
                    enabled,
                    state,
                    override_enabled: over,
                    width,
                    height,
                    min_width,
                    min_height,
                })
            },
        )?,
    )?;
    lua.globals().set("DockWidgetPluginGuiInfo", t)
}

/// Installs scalar value types and query parameter userdata for one world.
pub fn install(lua: &Lua, ctx: &WorldRef) -> Result<()> {
    let g = lua.globals();
    lua.set_app_data(ctx.clone());
    let reg = lua.create_table()?;
    reg.set(
        "new",
        lua.create_function(|lua, (a, b): (Value, Value)| {
            let a = vec3(a)?;
            let b = vec3(b)?;
            Ok(lua.create_userdata(Region3 { min: a, max: b })?)
        })?,
    )?;
    g.set("Region3", reg)?;
    let ray = lua.create_table()?;
    ray.set(
        "new",
        lua.create_function(|lua, (o, d): (Value, Value)| {
            Ok(lua.create_userdata(Ray {
                origin: vec3(o)?,
                direction: vec3(d)?,
            })?)
        })?,
    )?;
    g.set("Ray", ray)?;
    for (key, ty, dec, enc) in [
        (
            "$region3",
            "Region3",
            dec_region as fn(&Lua, &Json) -> Result<Value>,
            enc_region as fn(&AnyUserData) -> Result<Option<Json>>,
        ),
        ("$ray", "Ray", dec_ray, enc_ray),
        ("$font", "Font", dec_font, enc_font),
        ("$cell_id", "CellId", dec_cell, enc_cell),
    ] {
        values::register_codec(
            lua,
            Codec {
                key,
                type_name: ty,
                decode: dec,
                encode: enc,
            },
        )?;
    }
    values::register_codec(
        lua,
        Codec {
            key: "$axes",
            type_name: "Axes",
            decode: dec_direction,
            encode: enc_direction,
        },
    )?;
    values::register_codec(
        lua,
        Codec {
            key: "$faces",
            type_name: "Faces",
            decode: dec_direction,
            encode: enc_direction,
        },
    )?;
    install_font(lua)?;
    install_directions(lua)?;
    install_cell(lua)?;
    install_plugin_drag(lua)?;
    install_content(lua, ctx)?;
    install_catalog(lua)?;
    install_query_params(lua, ctx)?;
    Ok(())
}
#[cfg(test)]
#[path = "misc_values_tests.rs"]
mod tests;
fn dec_direction(lua: &Lua, j: &Json) -> Result<Value> {
    let (body, is_axes) = if let Some(b) = j.get("$axes") {
        (b, true)
    } else if let Some(b) = j.get("$faces") {
        (b, false)
    } else {
        return Err(err("direction set tag expected"));
    };
    let n = body
        .as_u64()
        .filter(|n| *n < 64)
        .ok_or_else(|| err("invalid direction mask"))?;
    if is_axes {
        Ok(Value::UserData(lua.create_userdata(AxesValue(
            DirectionSet {
                bits: n as u8,
                axes: true,
            },
        ))?))
    } else {
        Ok(Value::UserData(lua.create_userdata(FacesValue(
            DirectionSet {
                bits: n as u8,
                axes: false,
            },
        ))?))
    }
}
fn enc_direction(u: &AnyUserData) -> Result<Option<Json>> {
    if let Ok(v) = u.borrow::<AxesValue>() {
        return Ok(Some(json!({"$axes":v.0.bits})));
    }
    if let Ok(v) = u.borrow::<FacesValue>() {
        return Ok(Some(json!({"$faces":v.0.bits})));
    }
    Ok(None)
}
