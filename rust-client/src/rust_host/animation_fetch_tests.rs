use super::*;
use crate::rust_host;

fn setup() -> (Lua, WorldRef, scheduler::SchedulerRef) {
    let lua = Lua::new();
    let world = Rc::new(RefCell::new(world::World::from_snapshot(&json!({"version":1,"sequence":0,
        "services":{"Workspace":"workspace"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{}},
        {"id":"workspace","class_name":"Workspace","name":"Workspace","parent":"game","properties":{}}
    ]}),None).unwrap()));
    let scheduler = Rc::new(RefCell::new(scheduler::Scheduler::new()));
    scheduler::install(&lua, &scheduler).unwrap();
    rust_host::install_bindings(&lua, &world).unwrap();
    (lua, world, scheduler)
}
fn run(lua: &Lua, scheduler: &scheduler::SchedulerRef) {
    scheduler::step(lua, scheduler, Instant::now(), Duration::from_secs(1));
    let errors = scheduler::drain_errors(scheduler);
    assert!(errors.is_empty(), "{errors:?}");
}
fn start(lua: &Lua, scheduler: &scheduler::SchedulerRef, source: &str) -> mlua::Thread {
    let thread = scheduler::enqueue(
        lua,
        lua.load(source).into_function().unwrap(),
        MultiValue::new(),
    )
    .unwrap();
    run(lua, scheduler);
    thread
}
fn finish(lua: &Lua, ctx: &WorldRef, scheduler: &scheduler::SchedulerRef) -> usize {
    for frame in 1..2000 {
        ctx.borrow_mut().effects.clear();
        step(lua).unwrap();
        {
            let effects = &ctx.borrow().effects;
            assert!(
                effects
                    .get("mutations")
                    .and_then(Json::as_array)
                    .map_or(0, Vec::len)
                    <= NODES_PER_FRAME
            );
            assert!(serde_json::to_vec(effects).unwrap().len() < BYTES_PER_FRAME);
        }
        run(lua, scheduler);
        if state(lua).borrow().pending.is_empty() {
            return frame;
        }
    }
    panic!("animation request did not finish");
}
fn fixture() -> Json {
    animation_codec::decode(include_bytes!(
        "../../../tests/fixtures/animations/two-keyframes.rbxmx"
    ))
    .unwrap()
}
fn complete(lua: &Lua, clip: Json) {
    complete_loaded_animation_requests(lua, &json!([{"reference":"asset","clip":clip}])).unwrap();
}

#[test]
fn providers_return_independent_editable_trees_and_register_edits() {
    let (lua, ctx, scheduler) = setup();
    start(
        &lua,
        &scheduler,
        r#"
        local provider=game:GetService("AnimationClipProvider")
        local original=provider:GetAnimationClipAsync("asset")
        assert(original.Parent==nil and original:IsA("KeyframeSequence"))
        assert(original.Priority==Enum.AnimationPriority.Movement and original.Loop)
        local frames=original:GetKeyframes()
        assert(#frames==2 and frames[2].Time==2 and original.Length==2)
        local arm=frames[2]:GetPoses()[1]:GetSubPoses()[1]
        assert(arm.CFrame.X==4 and arm:GetSubPoses()[1].CFrame.X==2)
        assert(frames[2]:GetMarkers()[1].Value=="right")
        arm.CFrame=CFrame.new(9,0,0); arm.MaskWeight=0.25
        arm.EasingStyle=Enum.PoseEasingStyle.Cubic
        original.AuthoredHipHeight=2.5; original.Name="Edited"
        frames[2].Time=3
        assert(original.Length==3)
        local hash=provider:RegisterAnimationClip(original)
        assert(hash:sub(1,7)=="hash://")
        local copy=game:GetService("KeyframeSequenceProvider"):GetKeyframeSequenceAsync(hash)
        assert(copy.Name=="Edited" and copy.AuthoredHipHeight==2.5 and copy.Length==3)
        local pose=copy:GetKeyframes()[2]:GetPoses()[1]:GetSubPoses()[1]
        assert(pose.CFrame.X==9 and pose.MaskWeight==0.25 and pose.EasingStyle==Enum.PoseEasingStyle.Cubic)
        copy:Destroy()
        local again=provider:GetAnimationClipAsync("asset")
        assert(again~=original and again:GetKeyframes()[2].Root.Arm.CFrame.X==4)
        completed=true
    "#,
    );
    assert_eq!(
        ctx.borrow().effects["animation_requests"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    complete(&lua, fixture());
    finish(&lua, &ctx, &scheduler);
    assert!(lua.globals().get::<bool>("completed").unwrap());
}

#[test]
fn child_methods_filter_direct_children_and_keep_removed_nodes_alive() {
    let (lua, _, _) = setup();
    lua.load(
        r#"
        local s=Instance.new("KeyframeSequence")
        local k=Instance.new("Keyframe");s:AddKeyframe(k)
        local folder=Instance.new("Folder");s:AddKeyframe(folder)
        assert(folder.Parent==s and #s:GetKeyframes()==1)
        local nested=Instance.new("Keyframe",folder)
        assert(#s:GetKeyframes()==1)
        local p=Instance.new("Pose");k:AddPose(p)
        local child=Instance.new("Pose");p:AddSubPose(child)
        local m=Instance.new("KeyframeMarker");k:AddMarker(m)
        assert(#k:GetPoses()==1 and #k:GetMarkers()==1 and #p:GetSubPoses()==1)
        p:RemoveSubPose(child);k:RemoveMarker(m);k:RemovePose(p);s:RemoveKeyframe(k)
        assert(child.Parent==nil and m.Parent==nil and p.Parent==nil and k.Parent==nil)
        s:AddKeyframe(k);k:AddPose(p);p:AddSubPose(child)
        assert(#k:GetPoses()==1 and child.Parent==p)
        assert(not pcall(function() k:AddPose({}) end))
        assert(not pcall(function() child:AddSubPose(p) end))
        assert(not pcall(function() s:RemoveKeyframe(nil) end))
        m:Destroy();assert(not pcall(function() k:AddMarker(m) end))
    "#,
    )
    .exec()
    .unwrap();
}

#[test]
fn auxiliary_facial_and_binary_rig_data_survive_retrieval_and_registration() {
    let (lua, ctx, scheduler) = setup();
    let mut clip = fixture();
    clip["keyframes"][0]["poses"]
        .as_array_mut()
        .unwrap()
        .push(json!({"path":["Head"],"position":[0,0,0],"rotation":[1,0,0,0,1,0,0,0,1]}));
    clip["keyframes"][0]["number_poses"] = json!([{"path":["Head","FaceControls","Smile"],"value":0.75,"weight":0.5,"easing_style":2,"easing_direction":1}]);
    clip["rig_data"] = json!([{"name":"RigBytes","fields":{"name":{"bytes_hex":"00ff7f"},"transform":{"bytes_hex":"010002"}},"interpreted":false}]);
    start(
        &lua,
        &scheduler,
        r#"
        local p=game:GetService("AnimationClipProvider")
        local s=p:GetAnimationClipAsync("asset")
        local b=s.RigBytes
        assert(b.name==string.char(0,255,127) and b.transform==string.char(1,0,2))
        local n=s:GetKeyframes()[1].Head.FaceControls.Smile
        assert(n:IsA("NumberPose") and n.Value==0.75 and n.Weight==0.5)
        n.Value=0.25
        local copy=p:GetAnimationClipAsync(p:RegisterActiveAnimationClip(s))
        assert(copy.RigBytes.name==b.name)
        assert(copy:GetKeyframes()[1].Head.FaceControls.Smile.Value==0.25)
        completed=true
    "#,
    );
    complete(&lua, clip);
    finish(&lua, &ctx, &scheduler);
    assert!(lua.globals().get::<bool>("completed").unwrap());
}

fn large_clip(frames: usize) -> Json {
    let mut clip = fixture();
    let template = clip["keyframes"][0].clone();
    clip["keyframes"] = Json::Array(
        (0..frames)
            .map(|i| {
                let mut frame = template.clone();
                frame["time"] = json!(i);
                frame
            })
            .collect(),
    );
    clip["length"] = json!(frames - 1);
    clip
}
#[test]
fn large_result_is_built_across_bounded_frames() {
    let (lua, ctx, scheduler) = setup();
    start(
        &lua,
        &scheduler,
        r#"result=game:GetService("AnimationClipProvider"):GetAnimationClipAsync("asset");assert(#result:GetKeyframes()==400)"#,
    );
    complete(&lua, large_clip(400));
    assert!(finish(&lua, &ctx, &scheduler) > 10);
    assert!(lua.globals().get::<Value>("result").unwrap().is_userdata());
}
#[test]
fn cancelled_request_destroys_partial_tree_without_resuming_the_script() {
    let (lua, ctx, scheduler) = setup();
    let thread = start(
        &lua,
        &scheduler,
        r#"game:GetService("AnimationClipProvider"):GetAnimationClipAsync("asset");escaped=true"#,
    );
    complete(&lua, large_clip(400));
    ctx.borrow_mut().effects.clear();
    step(&lua).unwrap();
    let created = state(&lua).borrow().pending[0]
        .builder
        .as_ref()
        .unwrap()
        .created
        .clone();
    assert!(!created.is_empty());
    lua.globals().set("pending", thread).unwrap();
    lua.load("task.cancel(pending)").exec().unwrap();
    finish(&lua, &ctx, &scheduler);
    assert!(lua.globals().get::<Value>("escaped").unwrap().is_nil());
    assert!(created
        .iter()
        .all(|id| ctx.borrow().nodes.get(id).unwrap().destroyed));
}
#[test]
fn failures_and_timeouts_are_catchable_and_late_results_do_not_resume_twice() {
    let (lua, ctx, scheduler) = setup();
    start(
        &lua,
        &scheduler,
        r#"ok,reason=pcall(function() return game:GetService("AnimationClipProvider"):GetAnimationClipAsync("asset") end)"#,
    );
    complete_loaded_animation_requests(
        &lua,
        &json!([{"reference":"asset","error":"animation_download_failed"}]),
    )
    .unwrap();
    finish(&lua, &ctx, &scheduler);
    lua.load("assert(not ok and string.find(reason,'animation_download_failed'))")
        .exec()
        .unwrap();
    assert_eq!(
        ctx.borrow()
            .nodes
            .values()
            .filter(|n| n.class_name == "KeyframeSequence")
            .count(),
        0
    );
    start(
        &lua,
        &scheduler,
        r#"local p=game:GetService("AnimationClipProvider");ok,reason=pcall(function() return p:GetAnimationClipAsync("slow") end);finished=(finished or 0)+1"#,
    );
    state(&lua).borrow_mut().pending[0].deadline = Instant::now() - Duration::from_secs(1);
    finish(&lua, &ctx, &scheduler);
    complete_loaded_animation_requests(&lua, &json!([{"reference":"slow","clip":fixture()}]))
        .unwrap();
    finish(&lua, &ctx, &scheduler);
    lua.load("assert(not ok and string.find(reason,'timed out') and finished==1)")
        .exec()
        .unwrap();
}
#[test]
fn concurrent_waiters_share_a_request_and_enforce_the_pending_limit() {
    let (lua, ctx, scheduler) = setup();
    for _ in 0..32 {
        start(
            &lua,
            &scheduler,
            r#"game:GetService("AnimationClipProvider"):GetAnimationClipAsync("asset")"#,
        );
    }
    start(
        &lua,
        &scheduler,
        r#"ok,reason=pcall(function() game:GetService("AnimationClipProvider"):GetAnimationClipAsync("asset") end)"#,
    );
    lua.load("assert(not ok and string.find(reason,'too many pending'))")
        .exec()
        .unwrap();
    assert_eq!(
        ctx.borrow().effects["animation_requests"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    complete(&lua, fixture());
    finish(&lua, &ctx, &scheduler);
    assert_eq!(
        ctx.borrow()
            .nodes
            .values()
            .filter(|n| n.class_name == "KeyframeSequence" && !n.destroyed)
            .count(),
        32
    );
}
#[test]
fn equal_time_frames_keep_their_own_pose_sets() {
    let (lua, ctx, scheduler) = setup();
    let mut clip = fixture();
    clip["keyframes"][0]["poses"] =
        json!([{"path":["One"],"position":[1,0,0],"rotation":[1,0,0,0,1,0,0,0,1]}]);
    clip["keyframes"][1]["poses"] =
        json!([{"path":["Two"],"position":[2,0,0],"rotation":[1,0,0,0,1,0,0,0,1]}]);
    clip["keyframes"][1]["time"] = json!(0);
    clip["length"] = json!(0);
    start(
        &lua,
        &scheduler,
        r#"local s=game:GetService("AnimationClipProvider"):GetAnimationClipAsync("asset");local f=s:GetKeyframes();assert(#f[1]:GetPoses()==1 and f[1]:GetPoses()[1].Name=="One");assert(#f[2]:GetPoses()==1 and f[2]:GetPoses()[1].Name=="Two")"#,
    );
    complete(&lua, clip);
    finish(&lua, &ctx, &scheduler);
}

#[test]
fn timeout_is_not_starved_by_other_world_effects() {
    let (lua, ctx, scheduler) = setup();
    start(
        &lua,
        &scheduler,
        r#"ok,reason=pcall(function() game:GetService("AnimationClipProvider"):GetAnimationClipAsync("asset") end)"#,
    );
    state(&lua).borrow_mut().pending[0].deadline = Instant::now() - Duration::from_secs(1);
    world::emit(&ctx, "other_effects", json!("x".repeat(BYTES_PER_FRAME)));
    step(&lua).unwrap();
    run(&lua, &scheduler);
    assert!(state(&lua).borrow().pending.is_empty());
    lua.load("assert(not ok and reason:find('timed out'))")
        .exec()
        .unwrap();
}

#[test]
fn malformed_auxiliary_data_is_rejected_before_creating_a_result() {
    let (lua, ctx, scheduler) = setup();
    let mut clip = fixture();
    clip["rig_data"] = json!([{"fields":{"transform":{"bytes_hex":"0nothex"}}}]);
    start(
        &lua,
        &scheduler,
        r#"ok,reason=pcall(function() game:GetService("AnimationClipProvider"):GetAnimationClipAsync("asset") end)"#,
    );
    complete(&lua, clip);
    finish(&lua, &ctx, &scheduler);
    lua.load("assert(not ok and reason:find('invalid animation rig bytes'))")
        .exec()
        .unwrap();
    assert_eq!(
        ctx.borrow()
            .nodes
            .values()
            .filter(|n| n.class_name == "KeyframeSequence")
            .count(),
        0
    );
}

#[test]
fn descendant_walk_keeps_breadth_first_order_and_handles_changed_headers() {
    let snapshot=json!({"version":1,"sequence":0,"nodes":[
        {"id":"root","class_name":"Folder","parent":null},
        {"id":"x","class_name":"Folder","parent":"a"},
        {"id":"b","class_name":"Folder","parent":"root"},
        {"id":"a","class_name":"Folder","parent":"root"},
        {"id":"y","class_name":"Folder","parent":"b"},
        {"id":"z","class_name":"Folder","parent":"a"},
        {"id":"dead","class_name":"Folder","parent":"a","destroyed":true},
        {"id":"hidden","class_name":"Folder","parent":"dead"}
    ]});
    let ctx=Rc::new(RefCell::new(world::World::from_snapshot(&snapshot,None).unwrap()));
    assert_eq!(world::descendants(&ctx,"root"),vec!["b","a","y","x","z"]);
    ctx.borrow_mut().nodes.get_mut("x").unwrap().parent=Some("b".into());
    assert_eq!(world::descendants(&ctx,"root"),vec!["b","a","x","y","z"]);
    ctx.borrow_mut().nodes.get_mut("b").unwrap().destroyed=true;
    assert_eq!(world::descendants(&ctx,"root"),vec!["a","z"]);
    ctx.borrow_mut().nodes.get_mut("a").unwrap().parent=Some("z".into());
    assert_eq!(world::descendants(&ctx,"a"),vec!["z"]);
}
