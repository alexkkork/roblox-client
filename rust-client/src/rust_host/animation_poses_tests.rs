use super::*;
use crate::rust_host::{self, scheduler};
fn setup() -> (Lua, WorldRef) {
    let world=Rc::new(RefCell::new(world::World::from_snapshot(&json!({"version":1,"sequence":0,
        "services":{"Workspace":"workspace","RunService":"run"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{}},
        {"id":"workspace","class_name":"Workspace","name":"Workspace","parent":"game","properties":{}},
        {"id":"run","class_name":"RunService","name":"RunService","parent":"game","properties":{}}
    ]}),None).unwrap()));
    let lua = Lua::new();
    let scheduler = Rc::new(RefCell::new(scheduler::Scheduler::new()));
    scheduler::install(&lua, &scheduler).unwrap();
    rust_host::install_bindings(&lua, &world).unwrap();
    lua.load(r#"
        rig=Instance.new("Model",workspace);rig.Name="Rig"
        root=Instance.new("Part",rig);root.Name="Root";root.RootPriority=100
        arm=Instance.new("Part",rig);arm.Name="Arm";arm.CFrame=CFrame.new(4,0,0)
        joint=Instance.new("Motor6D",root);joint.Name="DifferentName";joint.Part0=root;joint.Part1=arm;joint.C0=CFrame.new(4,0,0)
        bone=Instance.new("Bone",arm);bone.Name="Bone";bone.CFrame=CFrame.new(0,2,0)
        controller=Instance.new("AnimationController",rig);animator=Instance.new("Animator",controller)
        function load(name)
            local a=Instance.new("Animation");a.AnimationId=name
            return animator:LoadAnimation(a)
        end
        track=load("test")
    "#).exec().unwrap();
    movement::flush_joint_poses(&lua, &world).unwrap();
    world.borrow_mut().effects.clear();
    (lua, world)
}
fn pose(path: &[&str], x: f64) -> Json {
    json!({"path":path,"position":[x,0.,0.],"rotation":[1.,0.,0.,0.,1.,0.,0.,0.,1.],"weight":1.})
}
fn clip(x: f64) -> Json {
    json!({"version":1,"kind":"KeyframeSequence","length":2.,"looped":false,"priority":1000,
    "keyframes":[{"time":0.,"name":"Start","markers":[],"poses":[pose(&["Root","Arm"],0.),pose(&["Root","Arm","Bone"],0.)]},
    {"time":2.,"name":"End","markers":[],"poses":[pose(&["Root","Arm"],x),pose(&["Root","Arm","Bone"],x/2.)]}]})
}
fn complete(lua: &Lua, reference: &str, value: Json) {
    super::super::complete_animation_requests(lua, &json!([{"reference":reference,"clip":value}]))
        .unwrap();
}

#[test]
fn unsupported_loaded_clips_fail_one_track_without_aborting_other_completions() {
    let (lua,ctx)=setup();
    lua.load("track:Play(0); good=load('good')").exec().unwrap();
    super::super::complete_loaded_animation_requests(&lua,&json!([
        {"reference":"test","clip":{"kind":"CurveAnimation"}},
        {"reference":"good","clip":clip(4.)}
    ])).unwrap();
    lua.load("assert(not track.IsPlaying); assert(good.Length==2); assert(not pcall(function() track:Play() end))").exec().unwrap();
    assert_eq!(ctx.borrow().effects["animation_load_errors"][0]["error"],"animation_clip_rejected");
}

#[test]
fn manual_clip_completion_wins_over_a_delayed_loader_result() {
    let (lua,_)=setup();
    complete(&lua,"test",clip(4.));
    super::super::complete_loaded_animation_requests(&lua,&json!([{"reference":"test","error":"animation_download_failed"}])).unwrap();
    lua.load("assert(track.Length==2);track:Play(0)").exec().unwrap();
}
fn step(lua: &Lua, ctx: &WorldRef, delta: f64) {
    super::super::step(lua, ctx, delta).unwrap();
    assert!(
        ctx.borrow().effects.get("animation_pose_errors").is_none(),
        "{:?}",
        ctx.borrow().effects
    );
}
#[test]
fn decoded_tracks_drive_motor_and_bone_then_move_parts_at_simulation() {
    let (lua, ctx) = setup();
    complete(&lua, "test", clip(4.));
    lua.load("track:Play(0)").exec().unwrap();
    step(&lua, &ctx, 1.);
    lua.load("assert(joint.Transform.X==2 and arm.Position.X==4);assert(bone.Transform.X==1)")
        .exec()
        .unwrap();
    movement::flush_joint_poses(&lua, &ctx).unwrap();
    lua.load("assert(root.Position.X==0 and arm.Position.X==6);assert(bone.TransformedWorldCFrame.X==7);assert(joint.C0.X==4)").exec().unwrap();
}
#[test]
fn tracks_blend_by_priority_per_joint_and_leave_neutral_remainder() {
    let (lua, ctx) = setup();
    complete(&lua, "test", clip(4.));
    lua.load("other=load('other')").exec().unwrap();
    let mut high = clip(12.);
    for frame in high["keyframes"].as_array_mut().unwrap() {
        frame["poses"].as_array_mut().unwrap().pop();
    }
    complete(&lua, "other", high);
    lua.load("track:Play(0,1,0);track.TimePosition=1;other.Priority=Enum.AnimationPriority.Action;other:Play(0,.25,0);other.TimePosition=1").exec().unwrap();
    step(&lua, &ctx, 0.);
    lua.load("assert(joint.Transform.X==3);assert(bone.Transform.X==1)")
        .exec()
        .unwrap();
    lua.load("track:AdjustWeight(.25,0);other:Stop(0)")
        .exec()
        .unwrap();
    step(&lua, &ctx, 0.);
    lua.load("assert(joint.Transform.X==.5 and bone.Transform.X==.25)")
        .exec()
        .unwrap();
}
#[test]
fn equal_priority_weights_normalize_and_ignore_lower_priority() {
    let (lua, ctx) = setup();
    complete(&lua, "test", clip(4.));
    lua.load("other=load('other')").exec().unwrap();
    complete(&lua, "other", clip(12.));
    lua.load("track:Play(0,2,0);track.TimePosition=1;other:Play(0,2,0);other.TimePosition=1")
        .exec()
        .unwrap();
    step(&lua, &ctx, 0.);
    lua.load("assert(joint.Transform.X==4 and bone.Transform.X==2)")
        .exec()
        .unwrap();
}
#[test]
fn stopping_fades_to_neutral_and_releases_ownership() {
    let (lua, ctx) = setup();
    complete(&lua, "test", clip(4.));
    lua.load("track:Play(0,1,0);track.TimePosition=1;track:Stop(1)")
        .exec()
        .unwrap();
    step(&lua, &ctx, 0.5);
    lua.load("assert(joint.Transform.X==1)").exec().unwrap();
    step(&lua, &ctx, 0.5);
    lua.load("assert(joint.Transform.X==0 and bone.Transform.X==0)")
        .exec()
        .unwrap();
    lua.load("joint.Transform=CFrame.new(9,0,0)")
        .exec()
        .unwrap();
    step(&lua, &ctx, 1.);
    lua.load("assert(joint.Transform.X==9)").exec().unwrap();
}
#[test]
fn loop_reverse_seek_and_pause_sample_the_track_position() {
    let (lua, ctx) = setup();
    complete(&lua, "test", clip(4.));
    lua.load("track.Looped=true;track:Play(0);track.TimePosition=1.75")
        .exec()
        .unwrap();
    step(&lua, &ctx, 0.5);
    lua.load("assert(track.TimePosition==.25 and joint.Transform.X==.5)")
        .exec()
        .unwrap();
    lua.load("track:AdjustSpeed(-1)").exec().unwrap();
    step(&lua, &ctx, 0.5);
    lua.load("assert(track.TimePosition==1.75 and joint.Transform.X==3.5);track:AdjustSpeed(0);track.TimePosition=.75").exec().unwrap();
    step(&lua, &ctx, 1.);
    lua.load("assert(joint.Transform.X==1.5)").exec().unwrap();
}
#[test]
fn names_rewire_bindings_and_stopped_tracks_preserve_new_script_writes() {
    let (lua, ctx) = setup();
    complete(&lua, "test", clip(4.));
    lua.load("track:Play(0,1,0);track.TimePosition=1")
        .exec()
        .unwrap();
    step(&lua, &ctx, 0.);
    lua.load("arm.Name='Renamed'").exec().unwrap();
    step(&lua, &ctx, 0.);
    lua.load("assert(joint.Transform.X==0);arm.Name='Arm'")
        .exec()
        .unwrap();
    step(&lua, &ctx, 0.);
    lua.load("assert(joint.Transform.X==2);track:Stop(0);joint.Transform=CFrame.new(7,0,0)")
        .exec()
        .unwrap();
    step(&lua, &ctx, 0.);
    lua.load("assert(joint.Transform.X==7)").exec().unwrap();
}
#[test]
fn invalid_response_is_atomic_and_can_be_retried() {
    let (lua, ctx) = setup();
    lua.load("other=load('other')").exec().unwrap();
    let mut bad = clip(4.);
    bad["keyframes"][1]["poses"][0]["rotation"] = json!(vec![0.; 9]);
    assert!(super::super::complete_animation_requests(
        &lua,
        &json!([
            {"reference":"test","clip":clip(4.)},{"reference":"other","clip":bad}
        ])
    )
    .is_err());
    lua.load("assert(track.Length==0 and other.Length==0)")
        .exec()
        .unwrap();
    complete(&lua, "test", clip(4.));
    lua.load("track:Play(0)").exec().unwrap();
    step(&lua, &ctx, 1.);
    lua.load("assert(joint.Transform.X==2)").exec().unwrap();
}
#[test]
fn destroyed_animator_releases_previous_pose() {
    let (lua, ctx) = setup();
    complete(&lua, "test", clip(4.));
    lua.load("track:Play(0,1,0);track.TimePosition=1")
        .exec()
        .unwrap();
    step(&lua, &ctx, 0.);
    lua.load("animator:Destroy()").exec().unwrap();
    step(&lua, &ctx, 0.);
    lua.load("assert(joint.Transform.X==0 and bone.Transform.X==0)")
        .exec()
        .unwrap();
}

fn actor_vm(ctx: &WorldRef) -> (Lua, WorldRef) {
    let actor = Rc::new(RefCell::new(ctx.borrow().fork_for_actor("worker")));
    let lua = Lua::new();
    let scheduler = Rc::new(RefCell::new(scheduler::Scheduler::new()));
    scheduler::install(&lua, &scheduler).unwrap();
    rust_host::install_bindings(&lua, &actor).unwrap();
    lua.load(
        r#"
        rig=workspace.Rig;root=rig.Root;arm=rig.Arm;joint=root.DifferentName;bone=arm.Bone
        animator=rig.AnimationController.Animator
        local animation=Instance.new("Animation");animation.AnimationId="actor-test"
        track=animator:LoadAnimation(animation)
        track:Play(0,1,0);track.TimePosition=1
    "#,
    )
    .exec()
    .unwrap();
    complete(&lua, "actor-test", clip(4.));
    (lua, actor)
}
#[test]
fn actor_animation_batch_replays_and_uses_current_root_position() {
    let (lua, ctx) = setup();
    let (alua, actor) = actor_vm(&ctx);
    step(&alua, &actor, 0.);
    movement::flush_joint_poses(&alua, &actor).unwrap();
    lua.load("root.CFrame=CFrame.new(10,0,0)").exec().unwrap();
    let journal = actor.borrow().effects["mutations"]
        .as_array()
        .unwrap()
        .clone();
    let result =
        rust_host::actor_shadow::merge_actor_journal(&lua, &ctx, &actor, "worker", &journal)
            .unwrap();
    assert!(result.conflicts.is_empty(), "{result:?}");
    lua.load("assert(root.Position.X==10 and arm.Position.X==16 and bone.Transform.X==1)")
        .exec()
        .unwrap();
}
#[test]
fn actor_animation_stale_names_reject_and_restore_the_whole_batch() {
    let (lua, ctx) = setup();
    let (alua, actor) = actor_vm(&ctx);
    step(&alua, &actor, 0.);
    movement::flush_joint_poses(&alua, &actor).unwrap();
    lua.load("arm.Name='NewName'").exec().unwrap();
    let journal = actor.borrow().effects["mutations"]
        .as_array()
        .unwrap()
        .clone();
    let result =
        rust_host::actor_shadow::merge_actor_journal(&lua, &ctx, &actor, "worker", &journal)
            .unwrap();
    assert!(
        result
            .conflicts
            .iter()
            .any(|v| v == "joint_animation:stale_input"),
        "{result:?}"
    );
    for vm in [&lua, &alua] {
        vm.load("assert(joint.Transform.X==0 and bone.Transform.X==0 and arm.Position.X==4)")
            .exec()
            .unwrap();
    }
}
#[test]
fn malformed_target_cannot_partially_update_a_rig() {
    let (lua, ctx) = setup();
    let joint = world::instance_id(&lua.globals().get::<Value>("joint").unwrap()).unwrap();
    let bone = world::instance_id(&lua.globals().get::<Value>("bone").unwrap()).unwrap();
    let mut good = identity();
    good.position[0] = 5.;
    let mut bad = identity();
    bad.rotation = [0.; 9];
    assert!(
        movement::apply_animation_poses(&lua, &ctx, &[(joint, good), (bone, bad)], &[]).is_err()
    );
    lua.load("assert(joint.Transform.X==0 and bone.Transform.X==0)")
        .exec()
        .unwrap();
}
#[test]
fn natural_completion_restores_pose_and_rotations_remain_rigid() {
    let (lua, ctx) = setup();
    let mut data = clip(4.);
    data["keyframes"][1]["poses"][0]["rotation"] = json!([-1., 0., 0., 0., -1., 0., 0., 0., 1.]);
    complete(&lua, "test", data);
    lua.load("track:Play(0)").exec().unwrap();
    step(&lua, &ctx, 1.);
    lua.load("assert(math.abs(joint.Transform.RightVector.Y-1)<.001)")
        .exec()
        .unwrap();
    movement::flush_joint_poses(&lua, &ctx).unwrap();
    step(&lua, &ctx, 1.);
    lua.load("assert(not track.IsPlaying and joint.Transform==CFrame.new())")
        .exec()
        .unwrap();
}

#[test]
fn local_registration_validates_enum_types_and_does_not_request_network_assets() {
    let (lua, ctx) = setup();
    ctx.borrow_mut().effects.clear();
    lua.load(
        r#"
        local provider=game:GetService("KeyframeSequenceProvider")
        assert(not pcall(function() provider:RegisterKeyframeSequence(root) end))
        local sequence=Instance.new("KeyframeSequence")
        local key=Instance.new("Keyframe",sequence);key.Time=2
        local pose=Instance.new("Pose",key);pose.Name="Root"
        assert(pose.EasingStyle==Enum.PoseEasingStyle.Linear)
        assert(pose.EasingDirection==Enum.PoseEasingDirection.In)
        assert(sequence.Priority==Enum.AnimationPriority.Core)
        assert(not pcall(function() pose.EasingStyle=Enum.EasingStyle.Linear end))
        assert(not pcall(function() sequence.Priority=Enum.Material.Plastic end))
        local hash=provider:RegisterKeyframeSequence(sequence)
        local active=provider:RegisterActiveKeyframeSequence(sequence)
        assert(string.sub(hash,1,7)=="hash://" and string.sub(active,1,9)=="active://")
        local a=Instance.new("Animation");a.AnimationId=hash
        local t=animator:LoadAnimation(a);assert(t.Length==2)
        key.Time=9;assert(t.Length==2)
    "#,
    )
    .exec()
    .unwrap();
    assert!(ctx.borrow().effects.get("animation_requests").is_none());
}
#[test]
fn separate_animators_do_not_bind_identical_names_in_other_models() {
    let (lua, ctx) = setup();
    complete(&lua, "test", clip(4.));
    lua.load(
        r#"
        local copy=rig:Clone();copy.Name="Rig2";copy.Parent=workspace
        local animation=Instance.new("Animation");animation.AnimationId="second"
        other=copy.AnimationController.Animator:LoadAnimation(animation)
        track:Play(0,1,0);track.TimePosition=1
        other:Play(0,1,0);other.TimePosition=.5
    "#,
    )
    .exec()
    .unwrap();
    complete(&lua, "second", clip(12.));
    step(&lua, &ctx, 0.);
    lua.load("assert(joint.Transform.X==2 and workspace.Rig2.Root.DifferentName.Transform.X==3)")
        .exec()
        .unwrap();
}
