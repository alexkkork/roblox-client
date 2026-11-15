use super::*;

#[test]
fn animation_completion_is_fenced_to_the_requesting_actor_generation() {
    let (root, world, pool) = setup();
    start_source(
        &root,
        &world,
        &pool,
        "worker",
        "=animation-loader",
        r#"
        local rig=Instance.new("Model",workspace)
        local controller=Instance.new("AnimationController",rig)
        local animator=Instance.new("Animator",controller)
        local a=Instance.new("Animation");a.AnimationId="rbxassetid://123"
        loadedTrack=animator:LoadAnimation(a)
    "#,
        Duration::from_secs(1),
    )
    .unwrap();
    let status = advance(&root, &world, &pool);
    let generation = status
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["actor_id"] == "worker")
        .unwrap()["runtime_instance_generation"]
        .as_u64()
        .unwrap();
    let clip = animation_codec::decode(include_bytes!(
        "../../../tests/fixtures/animations/two-keyframes.rbxmx"
    ))
    .unwrap();
    let mut response = json!({"actor_id":"worker","runtime_instance_generation":generation+1,"reference":"rbxassetid://123","clip":clip});
    super::super::super::playback::complete_loaded_animation_requests(
        &root,
        &json!([response.clone()]),
    )
    .unwrap();
    with_actor(&root, "worker", |lua, _, _| {
        assert_eq!(lua.load("return loadedTrack.Length").eval::<f64>()?, 0.);
        Ok(())
    })
    .unwrap();
    response["runtime_instance_generation"] = json!(generation);
    super::super::super::playback::complete_loaded_animation_requests(
        &root,
        &json!([response.clone()]),
    )
    .unwrap();
    // Repeated/late completion is harmless and cannot overwrite the installed clip.
    response["error"] = json!("late_failure");
    super::super::super::playback::complete_loaded_animation_requests(&root, &json!([response]))
        .unwrap();
    with_actor(&root, "worker", |lua, _, _| {
        assert_eq!(lua.load("return loadedTrack.Length").eval::<f64>()?, 2.);
        Ok(())
    })
    .unwrap();
}
