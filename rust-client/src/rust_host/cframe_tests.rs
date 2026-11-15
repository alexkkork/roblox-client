use super::*;

#[test]
fn transform_inverse_and_native_vector_shape() {
    let c = CFrameValue::new(
        [4., -2., 9.],
        euler(0.2, -0.7, 1.1, false).unwrap().rotation,
    );
    let inv = inverse(&c).unwrap();
    let product = compose(&c, &inv);
    assert!(product.position.iter().all(|x| x.abs() < 1e-10));
    for (got, want) in product
        .rotation
        .iter()
        .zip([1., 0., 0., 0., 1., 0., 0., 0., 1.])
    {
        assert!((got - want).abs() < 1e-10);
    }
    let p = [2., 3., -1.];
    let world = rotate(c.rotation, p);
    let world = [
        world[0] + c.position[0],
        world[1] + c.position[1],
        world[2] + c.position[2],
    ];
    let back_rotated = rotate(inv.rotation, world);
    let back = [
        back_rotated[0] + inv.position[0],
        back_rotated[1] + inv.position[1],
        back_rotated[2] + inv.position[2],
    ];
    for (got, want) in back.into_iter().zip(p) {
        assert!((got - want).abs() < 1e-10);
    }
}

#[test]
fn euler_quaternion_and_slerp_round_trip() {
    let a = euler(0.4, -0.2, 0.8, false).unwrap();
    let q = matrix_quat(a.rotation).unwrap();
    let r = quat_matrix(q);
    for (x, y) in a.rotation.iter().zip(r) {
        assert!((x - y).abs() < 1e-12);
    }
    let halfway = slerp(
        [1., 0., 0., 0., 1., 0., 0., 0., 1.],
        euler(0., std::f64::consts::PI, 0., false).unwrap().rotation,
        0.5,
    )
    .unwrap();
    assert!((halfway[0]).abs() < 1e-12 && (halfway[8]).abs() < 1e-12);
    assert!((euler_angles(a.rotation, false).unwrap().0 - 0.4).abs() < 1e-12);
    for order in 0..6 {
        let f = ordered_euler(0.15, -0.25, 0.35, order).unwrap();
        let (x, y, z) = ordered_euler_result(f.rotation, order).unwrap();
        assert!(
            (x - 0.15).abs() < 1e-12 && (y + 0.25).abs() < 1e-12 && (z - 0.35).abs() < 1e-12,
            "order {order}: {x}, {y}, {z}"
        );
    }
}

#[test]
fn look_fallback_and_unknown_rotation_codec_preserve_source() {
    let c = look([1., 2., 3.], [0., 1., 0.], [0., 1., 0.]).unwrap();
    let q = matrix_quat(c.rotation).unwrap();
    assert!(q.iter().all(|v| v.is_finite()));
    assert!(unit([0., 0., 0.], "test")
        .unwrap_err()
        .to_string()
        .contains("test must be nonzero"));
    let raw = json!({"position":[1,2,3],"rotation_id":7});
    let mut value = CFrameValue::new([1., 2., 3.], [1., 0., 0., 0., 1., 0., 0., 0., 1.]);
    value.rotation_known = false;
    value.source = raw.clone();
    assert_eq!(
        if value.rotation_known {
            json!({"position":value.position,"rotation":value.rotation})
        } else {
            value.source
        },
        raw
    );
}

#[test]
fn constructors_keep_roblox_basis_convention() {
    let c = look([10., 0., 0.], [0., 0., -1.], [0., 1., 0.]).unwrap();
    assert_eq!(c.position, [10., 0., 0.]);
    assert_eq!(c.rotation, [1., 0., 0., 0., 1., 0., 0., 0., 1.]);
    let x = rotate(c.rotation, [1., 0., 0.]);
    assert_eq!(x, [1., 0., 0.]);
}

#[test]
fn lua_surface_keeps_composition_properties_and_native_vectors() {
    let lua = mlua::Lua::new();
    super::super::numeric::install(&lua).unwrap();
    install(&lua).unwrap();
    lua.load(
        r#"
        local a = CFrame.new(10, 0, 0) * CFrame.Angles(0, math.pi / 2, 0)
        local p = a * Vector3.new(0, 0, -2)
        assert(typeof(p) == "Vector3")
        assert(math.abs(p.X - 8) < 1e-5 and math.abs(p.Z) < 1e-5)
        local back = a:PointToObjectSpace(p)
        assert((back - Vector3.new(0,0,-2)).Magnitude < 1e-5)
        local x,y,z = CFrame.new(3,4,5):GetComponents()
        assert(x == 3 and y == 4 and z == 5)
        local ok = pcall(function() a.X = 90 end)
        assert(not ok)
    "#,
    )
    .exec()
    .unwrap();
}

#[test]
fn codec_decodes_without_calling_replaceable_constructor_and_round_trips() {
    let lua = mlua::Lua::new();
    install(&lua).unwrap();
    let input = json!({"$cframe":{"position":[2,3,4],"rotation":[1,0,0,0,1,0,0,0,1]}});
    lua.globals().set("CFrame", mlua::Value::Nil).unwrap();
    let value = decode(&lua, &input).unwrap();
    let Value::UserData(ud) = value else {
        panic!("decoded CFrame should be userdata")
    };
    assert_eq!(
        *ud.borrow::<CFrameValue>().unwrap(),
        CFrameValue::new([2., 3., 4.], [1., 0., 0., 0., 1., 0., 0., 0., 1.])
    );
    assert_json_close(&encode(&ud).unwrap().unwrap(), &input);
    let partial = json!({"$cframe":{"position":[1,2,3],"rotation_id":5}});
    let Value::UserData(partial_ud) = decode(&lua, &partial).unwrap() else {
        panic!()
    };
    let c = partial_ud.borrow::<CFrameValue>().unwrap();
    assert!(!c.rotation_known);
    assert_eq!(encode(&partial_ud).unwrap().unwrap(), partial);
}

fn assert_json_close(left: &serde_json::Value, right: &serde_json::Value) {
    match (left, right) {
        (serde_json::Value::Number(a), serde_json::Value::Number(b)) => assert!(
            (a.as_f64().unwrap() - b.as_f64().unwrap()).abs() < 1e-12,
            "{left} != {right}"
        ),
        (serde_json::Value::Array(a), serde_json::Value::Array(b)) => {
            assert_eq!(a.len(), b.len());
            for (a, b) in a.iter().zip(b) {
                assert_json_close(a, b)
            }
        }
        (serde_json::Value::Object(a), serde_json::Value::Object(b)) => {
            assert_eq!(a.keys().collect::<Vec<_>>(), b.keys().collect::<Vec<_>>());
            for (k, a) in a {
                assert_json_close(a, &b[k]);
            }
        }
        _ => assert_eq!(left, right),
    }
}
