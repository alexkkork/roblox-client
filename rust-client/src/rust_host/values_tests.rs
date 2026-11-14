use super::*;
use mlua::UserData;

#[test]
fn json_round_trips_binary_string_and_buffer() -> mlua::Result<()> {
    let lua = Lua::new();
    let binary = lua.create_string([0x00, 0x7f, 0x80, 0xff])?;
    let encoded = to_json(&lua, &Value::String(binary))?;
    assert_eq!(encoded, serde_json::json!({"$bytes_hex":"007f80ff"}));
    let decoded = from_json(&lua, &encoded)?;
    let Value::String(decoded) = decoded else {
        panic!("expected a Lua string")
    };
    assert_eq!(&*decoded.as_bytes(), &[0x00, 0x7f, 0x80, 0xff]);
    assert_eq!(
        strict_result_json(&Value::String(decoded))?,
        serde_json::json!("\u{0000}\u{007f}ÿ")
    );

    let buffer = lua.create_buffer([0x10, 0x20, 0xff])?;
    let encoded = to_json(&lua, &Value::Buffer(buffer))?;
    assert_eq!(encoded, serde_json::json!({"$buffer_hex":"1020ff"}));
    let decoded = from_json(&lua, &encoded)?;
    let Value::Buffer(decoded) = decoded else {
        panic!("expected a Luau buffer")
    };
    assert_eq!(decoded.to_vec(), [0x10, 0x20, 0xff]);
    Ok(())
}

#[test]
fn vector_and_numeric_tags_keep_luau_shapes() -> mlua::Result<()> {
    let lua = Lua::new();
    let vector = from_json(&lua, &serde_json::json!({"$vector3":[1.25,-2,3]}))?;
    let Value::Vector(vector) = vector else {
        panic!("expected native Luau vector")
    };
    assert_eq!((vector.x(), vector.y(), vector.z()), (1.25, -2.0, 3.0));
    assert_eq!(
        to_json(&lua, &Value::Vector(vector))?,
        serde_json::json!({"$vector3":[1.25,-2.0,3.0]})
    );

    let nan = from_json(&lua, &serde_json::json!({"$number":"nan"}))?;
    assert!(matches!(nan, Value::Number(n) if n.is_nan()));
    assert_eq!(to_json(&lua, &nan)?, serde_json::json!({"$number":"nan"}));
    assert!(strict_result_json(&nan).is_err());
    assert_eq!(
        strict_result_json(&Value::Integer(i64::MIN))?,
        serde_json::json!(i64::MIN)
    );
    assert_eq!(
        strict_result_json(&Value::Number(7.0))?,
        serde_json::json!(7)
    );
    assert_eq!(
        strict_result_json(&Value::Number(9_007_199_254_740_992.0))?,
        serde_json::json!(9_007_199_254_740_992.0)
    );
    Ok(())
}

#[test]
fn rich_capture_keeps_luau_integer_variants_as_floating_wire_numbers() -> mlua::Result<()> {
    let lua = Lua::new();
    let integer = Value::Integer(42);
    assert_eq!(to_json(&lua, &integer)?, serde_json::json!(42.0));
    assert_eq!(result_json(&lua, &integer)?, serde_json::json!(42.0));

    let table = lua.create_table()?;
    table.set("value", 3)?;
    assert_eq!(
        to_json(&lua, &Value::Table(table))?,
        serde_json::json!({"value":3.0})
    );

    // The primitive-only standalone wire path is separate and keeps its
    // parsed integer token shape.
    assert_eq!(strict_result_json(&integer)?, serde_json::json!(42));
    Ok(())
}

#[test]
fn mutation_and_json_encode_keep_distinct_empty_table_shapes() -> mlua::Result<()> {
    let lua = Lua::new();
    let cycle = lua.create_table()?;
    cycle.set("self", cycle.clone())?;
    assert!(json_encode_value(&lua, &Value::Table(cycle)).is_err());

    let mixed = lua.create_table()?;
    mixed.set(1, "array")?;
    mixed.set("named", true)?;
    assert!(json_encode_value(&lua, &Value::Table(mixed.clone())).is_err());
    assert_eq!(
        mutation_json(&lua, &Value::Table(mixed))?,
        serde_json::json!({"1.000000":"array","named":true})
    );

    let hole = lua.create_table()?;
    hole.set(2, "second")?;
    assert!(json_encode_value(&lua, &Value::Table(hole)).is_err());

    let empty = lua.create_table()?;
    // `HttpService:JSONEncode` treats an empty Lua table as an array, while
    // world mutation capture uses the legacy object representation.
    assert_eq!(
        json_encode_value(&lua, &Value::Table(empty.clone()))?,
        serde_json::json!([])
    );
    assert_eq!(to_json(&lua, &Value::Table(empty))?, serde_json::json!({}));
    Ok(())
}

struct Wrapped(i64);
impl UserData for Wrapped {}

fn decode_wrapped(lua: &Lua, value: &Json) -> mlua::Result<Value> {
    let inner = value
        .get("$wrapped")
        .and_then(Json::as_i64)
        .ok_or_else(|| error("bad wrapped value"))?;
    Ok(Value::UserData(lua.create_userdata(Wrapped(inner))?))
}

fn encode_wrapped(value: &AnyUserData) -> mlua::Result<Option<Json>> {
    Ok(value
        .borrow::<Wrapped>()
        .ok()
        .map(|wrapped| serde_json::json!({"$wrapped":wrapped.0})))
}

#[test]
fn registered_codec_round_trips_without_holding_registry_borrow() -> mlua::Result<()> {
    let lua = Lua::new();
    register_codec(
        &lua,
        Codec {
            key: "$wrapped",
            type_name: "Wrapped",
            decode: decode_wrapped,
            encode: encode_wrapped,
        },
    )?;
    let value = from_json(&lua, &serde_json::json!({"$wrapped":42}))?;
    assert_eq!(to_json(&lua, &value)?, serde_json::json!({"$wrapped":42}));
    Ok(())
}
