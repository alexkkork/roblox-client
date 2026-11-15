//! Immutable Luau value types which are not part of the native vector family.
//!
//! These are plain Rust userdata. Decoding a received tag constructs the value
//! directly and never calls a global Lua constructor (which scripts can replace).

use mlua::{
    AnyUserData, Error, Lua, MetaMethod, Result, UserData, UserDataFields, UserDataMethods, Value,
    Variadic,
};
use serde_json::{json, Value as Json};

use super::values::{self, Codec};

const MAX_COMPONENT: f64 = 1.0e9;

#[cfg(test)]
#[path = "value_types_tests.rs"]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq)]
struct UDim {
    scale: f64,
    offset: i32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct UDim2 {
    x: UDim,
    y: UDim,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct NumberRange {
    min: f32,
    max: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Rect {
    min_x: f32,
    min_y: f32,
    max_x: f32,
    max_y: f32,
}

fn err(message: impl Into<String>) -> Error {
    Error::RuntimeError(message.into())
}
fn number(value: &Value) -> Result<f64> {
    match value {
        Value::Integer(n) => Ok(*n as f64),
        Value::Number(n) => Ok(*n),
        _ => Err(err("expected a number")),
    }
}
fn checked_udim(scale: f64, offset: f64) -> Result<UDim> {
    if !scale.is_finite()
        || scale.abs() > MAX_COMPONENT
        || !offset.is_finite()
        || offset.abs() > i32::MAX as f64
    {
        return Err(err("UDim components are out of range"));
    }
    Ok(UDim {
        scale,
        offset: offset.trunc() as i32,
    })
}
fn checked_float(value: f64, label: &str) -> Result<f32> {
    if !value.is_finite() || value.abs() > f32::MAX as f64 {
        return Err(err(format!("{label} must be a finite float")));
    }
    let out = value as f32;
    if !out.is_finite() {
        return Err(err(format!("{label} must be a finite float")));
    }
    Ok(out)
}
// Decoders are separate from the public constructors and intentionally bypass
// globals, protecting network snapshots from script monkey-patching.
fn decode_udim_value(value: &Json) -> Result<UDim> {
    let a = value
        .as_array()
        .filter(|a| a.len() == 2)
        .ok_or_else(|| err("invalid $udim payload"))?;
    checked_udim(
        a[0].as_f64().ok_or_else(|| err("invalid UDim scale"))?,
        a[1].as_f64().ok_or_else(|| err("invalid UDim offset"))?,
    )
}
fn tagged_payload<'a>(value: &'a Json, key: &str) -> Result<&'a Json> {
    value
        .as_object()
        .filter(|o| o.len() == 1)
        .and_then(|o| o.get(key))
        .ok_or_else(|| err(format!("invalid {key} value")))
}

impl UserData for UDim {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_meta_field("__type", "UDim");
        fields.add_field_method_get("Scale", |_, s| Ok(s.scale));
        fields.add_field_method_get("Offset", |_, s| Ok(s.offset));
    }
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method("__add", |_, a, b: mlua::AnyUserData| {
            let b = *b.borrow::<UDim>()?;
            checked_udim(a.scale + b.scale, a.offset as f64 + b.offset as f64)
        });
        methods.add_meta_method("__sub", |_, a, b: mlua::AnyUserData| {
            let b = *b.borrow::<UDim>()?;
            checked_udim(a.scale - b.scale, a.offset as f64 - b.offset as f64)
        });
        methods.add_meta_method("__eq", |_, a, b: mlua::AnyUserData| {
            Ok(b.borrow::<UDim>().map(|b| a == &*b).unwrap_or(false))
        });
        methods.add_meta_method("__tostring", |_, a, ()| {
            Ok(format!("{}, {}", a.scale, a.offset))
        });
        methods.add_meta_method(
            MetaMethod::NewIndex,
            |_, _, (key, _): (Value, Value)| -> Result<()> {
                Err(err(format!("UDim values are immutable ({key:?})")))
            },
        );
    }
}

impl UserData for UDim2 {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_meta_field("__type", "UDim2");
        fields.add_field_method_get("X", |lua, s| lua.create_userdata(s.x));
        fields.add_field_method_get("Width", |lua, s| lua.create_userdata(s.x));
        fields.add_field_method_get("Y", |lua, s| lua.create_userdata(s.y));
        fields.add_field_method_get("Height", |lua, s| lua.create_userdata(s.y));
    }
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method("__add", |_, a, b: mlua::AnyUserData| {
            let b = *b.borrow::<UDim2>()?;
            Ok(UDim2 {
                x: checked_udim(a.x.scale + b.x.scale, a.x.offset as f64 + b.x.offset as f64)?,
                y: checked_udim(a.y.scale + b.y.scale, a.y.offset as f64 + b.y.offset as f64)?,
            })
        });
        methods.add_meta_method("__sub", |_, a, b: mlua::AnyUserData| {
            let b = *b.borrow::<UDim2>()?;
            Ok(UDim2 {
                x: checked_udim(a.x.scale - b.x.scale, a.x.offset as f64 - b.x.offset as f64)?,
                y: checked_udim(a.y.scale - b.y.scale, a.y.offset as f64 - b.y.offset as f64)?,
            })
        });
        methods.add_meta_method("__eq", |_, a, b: mlua::AnyUserData| {
            Ok(b.borrow::<UDim2>().map(|b| a == &*b).unwrap_or(false))
        });
        methods.add_meta_method("__tostring", |_, a, ()| {
            Ok(format!(
                "{{{}, {}}}, {{{}, {}}}",
                a.x.scale, a.x.offset, a.y.scale, a.y.offset
            ))
        });
        methods.add_meta_method(
            MetaMethod::NewIndex,
            |_, _, (key, _): (Value, Value)| -> Result<()> {
                Err(err(format!("UDim2 values are immutable ({key:?})")))
            },
        );
        methods.add_method("Lerp", |lua, a, (goal, alpha): (mlua::AnyUserData, f64)| {
            let goal = *goal.borrow::<UDim2>()?;
            if !alpha.is_finite() || alpha.abs() > MAX_COMPONENT {
                return Err(err("UDim2:Lerp alpha must be finite and bounded"));
            }
            let lerp = |p: UDim, q: UDim| {
                checked_udim(
                    p.scale + (q.scale - p.scale) * alpha,
                    (p.offset as f64) + (q.offset as f64 - p.offset as f64) * alpha,
                )
            };
            lua.create_userdata(UDim2 {
                x: lerp(a.x, goal.x)?,
                y: lerp(a.y, goal.y)?,
            })
        });
    }
}

impl UserData for NumberRange {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_meta_field("__type", "NumberRange");
        fields.add_field_method_get("Min", |_, s| Ok(s.min));
        fields.add_field_method_get("Max", |_, s| Ok(s.max));
    }
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method("__eq", |_, a, b: mlua::AnyUserData| {
            Ok(b.borrow::<NumberRange>().map(|b| a == &*b).unwrap_or(false))
        });
        methods.add_meta_method("__tostring", |_, a, ()| Ok(format!("{}, {}", a.min, a.max)));
        methods.add_meta_method(
            MetaMethod::NewIndex,
            |_, _, (key, _): (Value, Value)| -> Result<()> {
                Err(err(format!("NumberRange values are immutable ({key:?})")))
            },
        );
    }
}

impl UserData for Rect {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_meta_field("__type", "Rect");
        fields.add_field_function_get("Min", |lua, ud| {
            let r = ud.borrow::<Rect>()?;
            super::numeric::push_vector2(lua, r.min_x as f64, r.min_y as f64)
        });
        fields.add_field_function_get("Max", |lua, ud| {
            let r = ud.borrow::<Rect>()?;
            super::numeric::push_vector2(lua, r.max_x as f64, r.max_y as f64)
        });
        fields.add_field_method_get("Width", |_, s| Ok(s.max_x - s.min_x));
        fields.add_field_method_get("Height", |_, s| Ok(s.max_y - s.min_y));
    }
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method("__eq", |_, a, b: mlua::AnyUserData| {
            Ok(b.borrow::<Rect>().map(|b| a == &*b).unwrap_or(false))
        });
        methods.add_meta_method("__tostring", |_, a, ()| {
            Ok(format!(
                "{}, {}, {}, {}",
                a.min_x, a.min_y, a.max_x, a.max_y
            ))
        });
        methods.add_meta_method(
            MetaMethod::NewIndex,
            |_, _, (key, _): (Value, Value)| -> Result<()> {
                Err(err(format!("Rect values are immutable ({key:?})")))
            },
        );
    }
}

fn decode_udim(lua: &Lua, value: &Json) -> Result<Value> {
    Ok(Value::UserData(lua.create_userdata(decode_udim_value(
        tagged_payload(value, "$udim")?,
    )?)?))
}
fn encode_udim(ud: &AnyUserData) -> Result<Option<Json>> {
    let Ok(v) = ud.borrow::<UDim>() else {
        return Ok(None);
    };
    Ok(Some(json!({"$udim":[v.scale,v.offset]})))
}
fn decode_udim2(lua: &Lua, value: &Json) -> Result<Value> {
    let a = tagged_payload(value, "$udim2")?
        .as_array()
        .filter(|a| a.len() == 2)
        .ok_or_else(|| err("invalid $udim2 payload"))?;
    let child = |x: &Json| -> Result<UDim> {
        let tagged = x.get("$udim").ok_or_else(|| err("invalid nested UDim"))?;
        decode_udim_value(tagged)
    };
    Ok(Value::UserData(lua.create_userdata(UDim2 {
        x: child(&a[0])?,
        y: child(&a[1])?,
    })?))
}
fn encode_udim2(ud: &AnyUserData) -> Result<Option<Json>> {
    let Ok(v) = ud.borrow::<UDim2>() else {
        return Ok(None);
    };
    Ok(Some(
        json!({"$udim2":[{"$udim":[v.x.scale,v.x.offset]},{"$udim":[v.y.scale,v.y.offset]}]}),
    ))
}
fn decode_range(lua: &Lua, value: &Json) -> Result<Value> {
    let a = tagged_payload(value, "$number_range")?
        .as_array()
        .filter(|a| a.len() == 2)
        .ok_or_else(|| err("invalid $number_range payload"))?;
    let min = checked_float(
        a[0].as_f64()
            .ok_or_else(|| err("invalid NumberRange minimum"))?,
        "NumberRange minimum",
    )?;
    let max = checked_float(
        a[1].as_f64()
            .ok_or_else(|| err("invalid NumberRange maximum"))?,
        "NumberRange maximum",
    )?;
    if min > max {
        return Err(err("NumberRange minimum must not exceed maximum"));
    }
    Ok(Value::UserData(
        lua.create_userdata(NumberRange { min, max })?,
    ))
}
fn encode_range(ud: &AnyUserData) -> Result<Option<Json>> {
    let Ok(v) = ud.borrow::<NumberRange>() else {
        return Ok(None);
    };
    Ok(Some(json!({"$number_range":[v.min,v.max]})))
}
fn decode_rect(lua: &Lua, value: &Json) -> Result<Value> {
    let a = tagged_payload(value, "$rect")?
        .as_array()
        .filter(|a| a.len() == 2)
        .ok_or_else(|| err("invalid $rect payload"))?;
    let xy = |j: &Json| -> Result<(f32, f32)> {
        let a = j
            .get("$vector2")
            .and_then(Json::as_array)
            .filter(|a| a.len() == 2)
            .ok_or_else(|| err("invalid Rect vector"))?;
        Ok((
            checked_float(
                a[0].as_f64().ok_or_else(|| err("invalid Rect x"))?,
                "Rect x",
            )?,
            checked_float(
                a[1].as_f64().ok_or_else(|| err("invalid Rect y"))?,
                "Rect y",
            )?,
        ))
    };
    let (ax, ay) = xy(&a[0])?;
    let (bx, by) = xy(&a[1])?;
    Ok(Value::UserData(lua.create_userdata(Rect {
        min_x: ax.min(bx),
        min_y: ay.min(by),
        max_x: ax.max(bx),
        max_y: ay.max(by),
    })?))
}
fn encode_rect(ud: &AnyUserData) -> Result<Option<Json>> {
    let Ok(v) = ud.borrow::<Rect>() else {
        return Ok(None);
    };
    Ok(Some(
        json!({"$rect":[{"$vector2":[v.min_x,v.min_y]},{"$vector2":[v.max_x,v.max_y]}]}),
    ))
}

fn make_ctor(
    lua: &Lua,
    global: &str,
    func: impl Fn(&Lua, Variadic<Value>) -> Result<Value> + 'static,
) -> Result<()> {
    let table = lua.create_table()?;
    table.set("new", lua.create_function(func)?)?;
    lua.globals().set(global, table)?;
    Ok(())
}

pub fn install(lua: &Lua) -> Result<()> {
    values::register_codec(
        lua,
        Codec {
            key: "$udim",
            type_name: "UDim",
            decode: decode_udim,
            encode: encode_udim,
        },
    )?;
    values::register_codec(
        lua,
        Codec {
            key: "$udim2",
            type_name: "UDim2",
            decode: decode_udim2,
            encode: encode_udim2,
        },
    )?;
    values::register_codec(
        lua,
        Codec {
            key: "$number_range",
            type_name: "NumberRange",
            decode: decode_range,
            encode: encode_range,
        },
    )?;
    values::register_codec(
        lua,
        Codec {
            key: "$rect",
            type_name: "Rect",
            decode: decode_rect,
            encode: encode_rect,
        },
    )?;
    values::register_codec(
        lua,
        Codec {
            key: "$physical_properties",
            type_name: "PhysicalProperties",
            decode: decode_physical,
            encode: encode_physical,
        },
    )?;
    make_ctor(lua, "UDim", |lua, args| {
        if args.len() > 2 {
            return Err(err("UDim.new accepts at most two numbers"));
        }
        let scale = if args.is_empty() {
            0.0
        } else {
            number(&args[0])?
        };
        let offset = if args.len() < 2 {
            0.0
        } else {
            number(&args[1])?
        };
        Ok(Value::UserData(
            lua.create_userdata(checked_udim(scale, offset)?)?,
        ))
    })?;
    make_ctor(lua, "UDim2", |lua, args| {
        let v = match args.len() {
            0 => UDim2 {
                x: UDim {
                    scale: 0.0,
                    offset: 0,
                },
                y: UDim {
                    scale: 0.0,
                    offset: 0,
                },
            },
            2 if args[0]
                .as_userdata()
                .and_then(|u| u.borrow::<UDim>().ok())
                .is_some()
                && args[1]
                    .as_userdata()
                    .and_then(|u| u.borrow::<UDim>().ok())
                    .is_some() =>
            {
                UDim2 {
                    x: *args[0].as_userdata().unwrap().borrow::<UDim>()?,
                    y: *args[1].as_userdata().unwrap().borrow::<UDim>()?,
                }
            }
            1..=4 => {
                let mut c = [0.0; 4];
                for (i, x) in args.iter().enumerate() {
                    c[i] = number(x)?;
                }
                UDim2 {
                    x: checked_udim(c[0], c[1])?,
                    y: checked_udim(c[2], c[3])?,
                }
            }
            _ => {
                return Err(err(
                    "UDim2.new accepts zero, two UDim values, or up to four numbers",
                ))
            }
        };
        Ok(Value::UserData(lua.create_userdata(v)?))
    })?;
    let udim2 = lua.globals().get::<mlua::Table>("UDim2")?;
    udim2.set(
        "fromScale",
        lua.create_function(|lua, (x, y): (f64, f64)| {
            Ok(lua.create_userdata(UDim2 {
                x: checked_udim(x, 0.0)?,
                y: checked_udim(y, 0.0)?,
            })?)
        })?,
    )?;
    udim2.set(
        "fromOffset",
        lua.create_function(|lua, (x, y): (f64, f64)| {
            Ok(lua.create_userdata(UDim2 {
                x: checked_udim(0.0, x)?,
                y: checked_udim(0.0, y)?,
            })?)
        })?,
    )?;
    make_ctor(lua, "NumberRange", |lua, args| {
        if !(1..=2).contains(&args.len()) {
            return Err(err("NumberRange.new expects one or two numbers"));
        }
        let min = checked_float(number(&args[0])?, "NumberRange minimum")?;
        let max = if args.len() == 1 {
            min
        } else {
            checked_float(number(&args[1])?, "NumberRange maximum")?
        };
        if min > max {
            return Err(err("NumberRange minimum must not exceed maximum"));
        }
        Ok(Value::UserData(
            lua.create_userdata(NumberRange { min, max })?,
        ))
    })?;
    make_ctor(lua, "Rect", |lua, args| {
        let (x1, y1, x2, y2) = match args.len() {
            4 => (
                number(&args[0])?,
                number(&args[1])?,
                number(&args[2])?,
                number(&args[3])?,
            ),
            2 => {
                let read = |v: &Value| -> Result<(f64, f64)> {
                    let ud = v
                        .as_userdata()
                        .ok_or_else(|| err("Rect.new expects Vector2 values"))?;
                    let point = *ud.borrow::<super::numeric::Vector2>()?;
                    Ok((point.x, point.y))
                };
                let (x1, y1) = read(&args[0])?;
                let (x2, y2) = read(&args[1])?;
                (x1, y1, x2, y2)
            }
            0 => (0.0, 0.0, 0.0, 0.0),
            _ => return Err(err("Rect.new expects two Vector2 values or four numbers")),
        };
        let (x1, y1, x2, y2) = (
            checked_float(x1, "Rect x")?,
            checked_float(y1, "Rect y")?,
            checked_float(x2, "Rect x")?,
            checked_float(y2, "Rect y")?,
        );
        Ok(Value::UserData(lua.create_userdata(Rect {
            min_x: x1.min(x2),
            min_y: y1.min(y2),
            max_x: x1.max(x2),
            max_y: y1.max(y2),
        })?))
    })?;
    // PhysicalProperties uses the existing shared clamping/default rules.
    make_ctor(lua, "PhysicalProperties", |lua, args| {
        let nums = args.iter().map(number).collect::<Result<Vec<_>>>()?;
        let p = crate::engine_physical_properties::PhysicalPropertiesValue::from_arguments(&nums)
            .map_err(|i| {
            if i == 0 {
                err("PhysicalProperties.new expects 3, 5, or 6 numbers")
            } else {
                err(format!("PhysicalProperties argument {i} must be finite"))
            }
        })?;
        lua.create_userdata(p).map(Value::UserData)
    })?;
    Ok(())
}

// PhysicalProperties already has an engine-level canonical value representation.
// The codec here follows its six-component authored tag.
impl UserData for crate::engine_physical_properties::PhysicalPropertiesValue {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_meta_field("__type", "PhysicalProperties");
        fields.add_field_method_get("Density", |_, s| Ok(s.density));
        fields.add_field_method_get("Friction", |_, s| Ok(s.friction));
        fields.add_field_method_get("Elasticity", |_, s| Ok(s.elasticity));
        fields.add_field_method_get("FrictionWeight", |_, s| Ok(s.friction_weight));
        fields.add_field_method_get("ElasticityWeight", |_, s| Ok(s.elasticity_weight));
        fields.add_field_method_get("AcousticAbsorption", |_, s| Ok(s.acoustic_absorption));
    }
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_meta_method("__eq", |_, a, b: AnyUserData| {
            Ok(b.borrow::<Self>().map(|b| a == &*b).unwrap_or(false))
        });
        methods.add_meta_method(
            MetaMethod::NewIndex,
            |_, _, (key, _): (Value, Value)| -> Result<()> {
                Err(err(format!(
                    "PhysicalProperties values are immutable ({key:?})"
                )))
            },
        );
    }
}
fn decode_physical(lua: &Lua, value: &Json) -> Result<Value> {
    let a = tagged_payload(value, "$physical_properties")?
        .as_array()
        .filter(|a| a.len() == 6)
        .ok_or_else(|| err("invalid $physical_properties payload"))?;
    let mut n = [0.0f64; 6];
    for i in 0..6 {
        n[i] = a[i]
            .as_f64()
            .ok_or_else(|| err("invalid PhysicalProperties component"))?;
    }
    let p = crate::engine_physical_properties::PhysicalPropertiesValue::from_six(n)
        .map_err(|_| err("PhysicalProperties component must be finite"))?;
    Ok(Value::UserData(lua.create_userdata(p)?))
}
fn encode_physical(ud: &AnyUserData) -> Result<Option<Json>> {
    let Ok(p) = ud.borrow::<crate::engine_physical_properties::PhysicalPropertiesValue>() else {
        return Ok(None);
    };
    Ok(Some(json!({"$physical_properties":p.components()})))
}
