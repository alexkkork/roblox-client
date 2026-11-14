use super::*;

fn setup() -> Lua {
    let lua = Lua::new();
    super::super::numeric::install(&lua).expect("numeric bindings install");
    install(&lua).expect("value bindings install");
    lua
}

#[test]
fn udim_and_udim2_constructors_arithmetic_and_tags_match_wire_shape() {
    let lua = setup();
    let value: Value = lua.load("local a=UDim.new(0.25, 10.8); local b=UDim2.fromOffset(4, 8); return UDim2.new(a, UDim.new(0.5, -3)) + b").eval().unwrap();
    let json = values::to_json(&lua, &value).unwrap();
    assert_eq!(
        json,
        json!({"$udim2":[{"$udim":[0.25,14]},{"$udim":[0.5,5]}]})
    );
    let rebuilt = values::from_json(&lua, &json).unwrap();
    assert_eq!(values::to_json(&lua, &rebuilt).unwrap(), json);
    let fields: (f64, i32) = lua
        .load("local u=UDim.new(0.5, 4); return u.Scale,u.Offset")
        .eval()
        .unwrap();
    assert_eq!(fields, (0.5, 4));
}

#[test]
fn number_range_rejects_reversed_bounds_and_round_trips() {
    let lua = setup();
    let value: Value = lua.load("return NumberRange.new(1.25, 8)").eval().unwrap();
    let json = values::to_json(&lua, &value).unwrap();
    assert_eq!(json, json!({"$number_range":[1.25,8.0]}));
    assert!(lua
        .load("return NumberRange.new(2, 1)")
        .eval::<Value>()
        .is_err());
    assert_eq!(
        values::to_json(&lua, &values::from_json(&lua, &json).unwrap()).unwrap(),
        json
    );
}

#[test]
fn rect_sorts_bounds_returns_vector2_and_round_trips() {
    let lua = setup();
    let value: Value = lua
        .load("return Rect.new(Vector2.new(8, 9), Vector2.new(-2, 3))")
        .eval()
        .unwrap();
    let json = values::to_json(&lua, &value).unwrap();
    assert_eq!(
        json,
        json!({"$rect":[{"$vector2":[-2.0,3.0]},{"$vector2":[8.0,9.0]}]})
    );
    let got: (f32, f32, f64, f64) = lua
        .load("local r=Rect.new(8,9,-2,3); return r.Min.X,r.Min.Y,r.Width,r.Height")
        .eval()
        .unwrap();
    assert_eq!(got, (-2.0, 3.0, 10.0, 6.0));
    assert_eq!(
        values::to_json(&lua, &values::from_json(&lua, &json).unwrap()).unwrap(),
        json
    );
}

#[test]
fn physical_properties_preserves_defaults_clamps_and_wire_tag() {
    let lua = setup();
    let value: Value = lua
        .load("return PhysicalProperties.new(-2, 3, 0.5)")
        .eval()
        .unwrap();
    let json = values::to_json(&lua, &value).unwrap();
    assert_eq!(
        json,
        json!({"$physical_properties":[0.0001f32,2.0f32,0.5f32,1.0f32,1.0f32,0.0f32]})
    );
    let rebuilt = values::from_json(&lua, &json).unwrap();
    assert_eq!(values::to_json(&lua, &rebuilt).unwrap(), json);
}

#[test]
fn snapshot_decode_does_not_call_replaced_global_constructors() {
    let lua = setup();
    lua.load("UDim={new=function() error('must not run') end}; NumberRange={new=function() error('must not run') end}").exec().unwrap();
    for json in [json!({"$udim":[0.5,3]}), json!({"$number_range":[1.0,2.0]})] {
        let v = values::from_json(&lua, &json).unwrap();
        assert_eq!(values::to_json(&lua, &v).unwrap(), json);
    }
}

#[test]
fn value_userdata_is_readonly_and_overloads_reject_invalid_shapes() {
    let lua = setup();
    let checks: (bool, bool, bool, bool, bool, bool, bool) = lua
        .load(
            r#"
            local u=UDim.new(1, 2)
            local n=NumberRange.new(1, 2)
            local r=Rect.new(1, 2, 3, 4)
            local p=PhysicalProperties.new(1, 0.5, 0.2)
            local function readonly(v, key)
                return not pcall(function() v[key]=0 end)
            end
            return
                readonly(u, "Scale"), readonly(n, "Min"), readonly(r, "Width"),
                readonly(p, "Density"),
                not pcall(function() NumberRange.new() end),
                not pcall(function() Rect.new(Vector3.new(1, 2, 3), Vector3.new(4, 5, 6)) end),
                not pcall(function() PhysicalProperties.new(1, 2) end)
            "#,
        )
        .eval()
        .unwrap();
    assert_eq!(checks, (true, true, true, true, true, true, true));
}
