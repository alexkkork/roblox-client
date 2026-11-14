use super::{numeric, values};
use mlua::{Lua, Value};
use serde_json::json;

fn runtime() -> Lua {
    let lua = Lua::new();
    numeric::install(&lua).expect("numeric bindings install");
    lua
}

#[test]
fn vector2_userdata_properties_arithmetic_and_immutability() {
    let lua = runtime();
    let result: (f64, f64, f64, f64, bool, String) = lua
        .load(
            r#"
            local a, b = Vector2.new(3, 4), Vector2.new(-1, 2)
            local c = a + b
            local d = a * 2
            local ok = pcall(function() a.X = 8 end)
            return a.Magnitude, c.X, c.Y, d.Y, not ok, tostring(a)
        "#,
        )
        .eval()
        .expect("Vector2 probe");
    assert_eq!(result.0, 5.0);
    assert_eq!((result.1, result.2, result.3), (2.0, 6.0, 8.0));
    assert!(result.4);
    assert_eq!(result.5, "3, 4");
}

#[test]
fn color3_constructors_hsv_hex_and_wire_round_trip() {
    let lua = runtime();
    let (h, s, v, text): (f64, f64, f64, String) = lua
        .load("local c=Color3.fromHex('#f00'); local h,s,v=c:ToHSV(); return h,s,v,c:ToHex()")
        .eval()
        .expect("Color3 probe");
    assert_eq!((h, s, v), (1.0, 1.0, 1.0));
    assert_eq!(text, "ff0000");
    let tagged = json!({"$color3":[0.25,0.5,1.0]});
    let decoded = values::from_json(&lua, &tagged).expect("decode color");
    let encoded = values::to_json(&lua, &decoded).expect("encode color");
    assert_eq!(encoded, tagged);
}

#[test]
fn signed_16_bit_vector_constructors_match_float_and_wrap_semantics() {
    let lua = runtime();
    let value: (i32, i32, i32) = lua
        .load("local v=Vector3int16.new(-1.9,32768,65537); return v.X,v.Y,v.Z")
        .eval()
        .expect("int16 constructor probe");
    assert_eq!(value, (-1, -32768, 1));
    let division: i32 = lua
        .load("return (Vector2int16.new(9, 8) / 2).X")
        .eval()
        .expect("integer division narrows after float division");
    assert_eq!(division, 4);
    let (scalar_order, type2, type3): (i32, String, String) = lua
        .load(
            "local v=Vector3int16.new(5,7,9); return (0.5*v).X, typeof(Vector2int16.new()), typeof(v)",
        )
        .eval()
        .expect("float32 scalar and Luau type metadata");
    assert_eq!(scalar_order, 0);
    assert_eq!(type2, "Vector2int16");
    assert_eq!(type3, "Vector3int16");
}

#[test]
fn tagged_integer_and_region_values_round_trip() {
    let lua = runtime();
    for tagged in [
        json!({"$vector2int16":[-2,7]}),
        json!({"$vector3int16":[-2,7,12]}),
        json!({"$region3int16":[[-2,7,12],[4,5,6]]}),
    ] {
        let value = values::from_json(&lua, &tagged).expect("decode integer type");
        assert!(matches!(value, Value::UserData(_)));
        assert_eq!(
            values::to_json(&lua, &value).expect("encode integer type"),
            tagged
        );
    }
}

#[test]
fn vector3_remains_native_luau_value_with_f32_rounding() {
    let lua = runtime();
    let native = lua
        .load("return Vector3.new(16777217,2,3)")
        .eval::<Value>()
        .expect("native Vector3 probe");
    let Value::Vector(native) = native else {
        panic!("Vector3.new must return Luau's native vector value");
    };
    assert_eq!(native.x(), 16_777_216.0);
    let component: f64 = lua
        .load("return Vector3.new(16777217,2,3).X")
        .eval()
        .expect("Vector3 component probe");
    assert_eq!(component, 16_777_216.0);
}
