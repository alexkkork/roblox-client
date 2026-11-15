use super::*;

fn lua() -> Lua {
    let lua = Lua::new();
    super::super::enums::install(&lua).unwrap();
    super::super::numeric::install(&lua).unwrap();
    super::super::cframe::install(&lua).unwrap();
    install(&lua).unwrap();
    lua
}

#[test]
fn number_sequence_builds_points_and_roundtrips_wire_value() {
    let lua = lua();
    let result: String = lua
        .load(
            r#"
      local a=NumberSequence.new({NumberSequenceKeypoint.new(0,2),NumberSequenceKeypoint.new(1,5)})
      return tostring(a)..":"..tostring(a.Keypoints[2])
    "#,
        )
        .eval()
        .unwrap();
    assert_eq!(result, "NumberSequence (2 keypoints):1, 5, 0");
    let json = json!({"$number_sequence":[
        {"$number_keypoint":[0.0,2.0,0.0]},
        {"$number_keypoint":[1.0,5.0,0.0]}
    ]});
    let value = values::from_json(&lua, &json).unwrap();
    assert_eq!(values::to_json(&lua, &value).unwrap(), json);
}

#[test]
fn sequence_rejects_bad_order_endpoint_and_sparse_array() {
    let lua = lua();
    for code in [
      "NumberSequence.new({NumberSequenceKeypoint.new(0,0),NumberSequenceKeypoint.new(0.5,1)})",
      "NumberSequence.new({NumberSequenceKeypoint.new(0,0),NumberSequenceKeypoint.new(0.8,1),NumberSequenceKeypoint.new(0.7,2),NumberSequenceKeypoint.new(1,3)})",
      "local t={[1]=NumberSequenceKeypoint.new(0,0),[3]=NumberSequenceKeypoint.new(1,1)}; return NumberSequence.new(t)",
    ] { assert!(lua.load(code).eval::<Value>().is_err(),"should reject {code}"); }
}

#[test]
fn color_sequence_preserves_color_and_keypoint_immutability() {
    let lua = lua();
    let n: bool = lua
        .load(
            r#"
      local c=ColorSequence.new(Color3.new(1,0,0),Color3.new(0,0,1))
      return c.Keypoints[1].Value.R==1 and c.Keypoints[2].Value.B==1
    "#,
        )
        .eval()
        .unwrap();
    assert!(n);
    assert!(lua
        .load("local p=NumberSequenceKeypoint.new(0,1); p.Value=3")
        .exec()
        .is_err());
}

#[test]
fn tween_info_defaults_and_wire_validation() {
    let lua = lua();
    let values:(f64,i32,i32,i32,bool,f64)=lua.load("local t=TweenInfo.new(); return t.Time,t.EasingStyle.Value,t.EasingDirection.Value,t.RepeatCount,t.Reverses,t.DelayTime").eval().unwrap();
    assert_eq!(values, (1., 3, 1, 0, false, 0.));
    let wire = json!({"$tween_info":[2.0,10,2,-2,true,0.25]});
    let v = values::from_json(&lua, &wire).unwrap();
    assert_eq!(values::to_json(&lua, &v).unwrap(), wire);
}

#[test]
fn brickcolor_resolves_name_number_and_rgb() {
    let lua = lua();
    let n:(i32,String,i32)=lua.load("return BrickColor.new('Bright red').Number, BrickColor.new(21).Name, BrickColor.new(1,0,0).Number").eval().unwrap();
    assert_eq!(n, (21, "Bright red".into(), 1004));
    let wire = json!({"$brickcolor":194});
    let v = values::from_json(&lua, &wire).unwrap();
    assert_eq!(values::to_json(&lua, &v).unwrap(), wire);
    let all_palette_slots: bool = lua
        .load(
            r#"
            for i = 0, 127 do
                local color = BrickColor.palette(i)
                assert(BrickColor.new(color.Number).Name == color.Name)
                assert(color.Color == Color3.new(color.r, color.g, color.b))
            end
            assert(BrickColor.palette(0).Number == 141)
            assert(BrickColor.palette(127).Number == 1003)
            return true
        "#,
        )
        .eval()
        .unwrap();
    assert!(all_palette_slots);
}

#[test]
fn waypoint_label_keeps_arbitrary_bytes() {
    let lua = lua();
    let action = super::super::enums::enum_item(&lua, "PathWaypointAction", "Jump").unwrap();
    let value = lua
        .create_userdata(PathWaypoint {
            position: Vector::new(1., 2., 3.),
            action: 1,
            label: vec![0xff, 0x00],
        })
        .unwrap();
    let wire = values::to_json(&lua, &Value::UserData(value)).unwrap();
    assert_eq!(wire["$path_waypoint"]["label_hex"], "ff00");
    assert_eq!(wire["$path_waypoint"]["action"], 1);
    drop(action);
}

#[test]
fn rotation_key_mutations_and_codec_preserve_tangents() {
    let lua = lua();
    let wire = json!({"$rotation_curve_key":[0.5,{"position":[1.0,2.0,3.0],"rotation":[1.0,0.0,0.0,0.0,1.0,0.0,0.0,0.0,1.0]},2,-1.25,3.5]});
    let value = values::from_json(&lua, &wire).unwrap();
    assert_eq!(values::to_json(&lua, &value).unwrap(), wire);
    let result: (f64, i32, f64, bool) = lua
        .load(
            r#"
      local key=RotationCurveKey.new(0.25,CFrame.new(1,2,3),Enum.KeyInterpolationMode.Cubic)
      key.LeftTangent=-2
      key.RightTangent=4
      key.Interpolation=Enum.KeyInterpolationMode.Linear
      return key.Time,key.Interpolation.Value,key.LeftTangent,key.RightTangent==nil
    "#,
        )
        .eval()
        .unwrap();
    assert_eq!(result, (0.25, 1, -2.0, true));
}
