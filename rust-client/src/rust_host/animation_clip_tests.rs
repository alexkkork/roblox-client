use super::*;
use serde_json::json;
fn input() -> Json {
    json!({"keyframes":[
        {"time":0.,"poses":[{"path":["Root","Arm"],"position":[0.,0.,0.],"rotation":[1.,0.,0.,0.,1.,0.,0.,0.,1.]}]},
        {"time":2.,"poses":[{"path":["Root","Arm"],"position":[4.,0.,0.],"rotation":[-1.,0.,0.,0.,-1.,0.,0.,0.,1.]}]}
    ]})
}
#[test]
fn sparse_pose_channels_lerp_positions_and_slerp_rotations() {
    let clip = Clip::compile(&input()).unwrap();
    let (f, w) = clip.channels[0].sample(1.).unwrap();
    assert_eq!(f.position, [2., 0., 0.]);
    assert_eq!(w, 1.);
    assert!(f.rotation[0].abs() < 1e-9 && (f.rotation[3] - 1.).abs() < 1e-9);
    assert!(clip["keyframes"][0].get("poses").is_none());
    assert_eq!(clip.channels[0].sample(-1.).unwrap().0.position, [0.; 3]);
    assert_eq!(
        clip.channels[0].sample(3.).unwrap().0.position,
        [4., 0., 0.]
    );
}

#[test]
fn dense_body_clip_uses_compact_channels_without_retaining_pose_json() {
    let frames=(0..210).map(|time|json!({"time":time as f64/30.0,"name":"Frame","markers":[],
        "poses":(0..100).map(|channel|json!({"path":["Root",format!("Joint{channel}")],"position":[time as f64,0.,0.],
            "rotation":[1.,0.,0.,0.,1.,0.,0.,0.,1.]})).collect::<Vec<_>>()})).collect::<Vec<_>>();
    let clip = Clip::compile(&json!({"keyframes":frames})).unwrap();
    assert_eq!(clip.channels.len(), 100);
    assert_eq!(clip.channels[0].keys.len(), 210);
    assert!((clip.channels[0].sample(1.5).unwrap().0.position[0] - 45.).abs() < 1e-6);
    assert!(clip["keyframes"]
        .as_array()
        .unwrap()
        .iter()
        .all(|f| f.get("poses").is_none()));
}
#[test]
fn constant_keys_hold_until_the_next_key() {
    let mut value = input();
    value["keyframes"][0]["poses"][0]["easing_style"] = json!(1);
    let clip = Clip::compile(&value).unwrap();
    assert_eq!(clip.channels[0].sample(1.99).unwrap().0.position, [0.; 3]);
    assert_eq!(
        clip.channels[0].sample(2.).unwrap().0.position,
        [4., 0., 0.]
    );
}
#[test]
fn invalid_frames_paths_enums_and_duplicate_keys_are_rejected() {
    for (field, value) in [
        ("path", json!([])),
        ("position", json!([0., 0.])),
        ("rotation", json!(vec![0.; 9])),
        ("weight", json!(-1)),
        ("easing_style", json!(9)),
        ("easing_direction", json!(9)),
    ] {
        let mut clip = input();
        clip["keyframes"][0]["poses"][0][field] = value;
        assert!(Clip::compile(&clip).is_err(), "{field}");
    }
    let mut clip = input();
    clip["keyframes"][1]["time"] = json!(0.);
    assert!(Clip::compile(&clip).is_err());
}
#[test]
fn cubic_v2_and_legacy_directions_are_distinct() {
    assert!((pose_ease(5, 0, 0.25) - 0.015625).abs() < 1e-12);
    assert!((pose_ease(3, 0, 0.25) - 0.578125).abs() < 1e-12);
    for style in 0..=5 {
        for direction in 0..=2 {
            assert_eq!(pose_ease(style, direction, 0.), 0.);
            assert_eq!(pose_ease(style, direction, 1.), 1.);
        }
    }
}
