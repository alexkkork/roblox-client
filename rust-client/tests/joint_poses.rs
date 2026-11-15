#![cfg(feature = "rust-host")]
use headless_luau::runtime_owner::Runtime;
use serde_json::{json, Value};
use std::time::{Duration, Instant};
fn runtime() -> Runtime {
    Runtime::new(json!({"version":1,"sequence":1,"services":{"Workspace":"workspace"},"nodes":[{"id":"workspace","class_name":"Workspace","name":"Workspace","parent":null,"properties":{}}]}),32).unwrap()
}
fn check(reply: &Value) {
    assert_ne!(reply["ok"], false, "{reply}");
    for key in ["task_errors", "joint_pose_errors"] {
        assert!(
            reply[key].as_array().is_none_or(|v| v.is_empty()),
            "{reply}"
        );
    }
    if let Some(actors) = reply["actor_runtime"].as_array() {
        for actor in actors {
            check(actor);
            check(&actor["world_effects"]);
        }
    }
}
fn execute(r: &mut Runtime, source: &str) -> Value {
    let reply = r
        .call(json!({"op":"execute","source":format!("{HELPERS}\n{source}"),"timeout_ms":2000}))
        .unwrap();
    check(&reply);
    let id = reply["job_id"]
        .as_u64()
        .unwrap_or_else(|| panic!("{reply}"));
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        let reply = r.status(id).unwrap();
        check(&reply);
        if reply["completed"] == true {
            return reply;
        }
        assert!(Instant::now() < end, "{reply}");
        check(&r.poll().unwrap());
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn run(source: &str) {
    execute(&mut runtime(), source);
}

#[test]
fn packaged_joint_pose_smoke_runs_with_official_luau() {
    let result = execute(
        &mut runtime(),
        include_str!("../../examples/joint-poses-smoke.luau"),
    );
    assert_eq!(
        result["values"][0]["passed"].as_f64(),
        Some(18.),
        "{result}"
    );
}
const HELPERS: &str = r###"
local function vec(a,b) assert((a-b).Magnitude<1e-3,tostring(a).." ~= "..tostring(b)) end
local function frame(a,b)
 local aa,bb={a:GetComponents()},{b:GetComponents()}
 for i=1,12 do assert(math.abs(aa[i]-bb[i])<1e-3,tostring(i)..": "..tostring(aa[i]).." ~= "..tostring(bb[i])) end
end
"###;

#[test]
fn c0_immediately_moves_the_child_branch_and_keeps_root_fixed() {
    run(r###"
local function part(name,x)
 local p=Instance.new("Part",workspace);p.Name=name;p.CFrame=CFrame.new(x,0,0);return p
end
local a,b,c=part("A",0),part("B",4),part("C",8)
a.RootPriority=100
local ab=Instance.new("Motor6D",a);ab.Part0=a;ab.Part1=b;ab.C0=CFrame.new(4,0,0)
local bc=Instance.new("Motor6D",b);bc.Part0=b;bc.Part1=c;bc.C0=CFrame.new(4,0,0)

local root=a.CFrame
ab.C0=CFrame.new(6,2,3)*CFrame.Angles(.2,.3,.4)
frame(a.CFrame,root)
frame(b.CFrame,a.CFrame*ab.C0*ab.Transform*ab.C1:Inverse())
frame(c.CFrame,b.CFrame*bc.C0*bc.Transform*bc.C1:Inverse())
"###);
}

#[test]
fn c1_and_reversed_root_move_the_correct_side() {
    run(r###"
local function part(name,x)
 local p=Instance.new("Part",workspace);p.Name=name;p.CFrame=CFrame.new(x,0,0);return p
end
local a,b,c=part("A",0),part("B",4),part("C",8)
a.RootPriority=100
local ab=Instance.new("Motor6D",a);ab.Part0=a;ab.Part1=b;ab.C0=CFrame.new(4,0,0)
local bc=Instance.new("Motor6D",b);bc.Part0=b;bc.Part1=c;bc.C0=CFrame.new(4,0,0)

c.RootPriority=127;local root=c.CFrame;local oldB=b.CFrame
ab.C1=CFrame.new(1,2,3)*CFrame.Angles(.2,.4,.6)
frame(c.CFrame,root);frame(b.CFrame,oldB)
frame(a.CFrame,b.CFrame*ab.C1*(ab.C0*ab.Transform):Inverse())
bc.C0=CFrame.new(10,0,0)*CFrame.Angles(.1,.3,.2)
frame(c.CFrame,root)
frame(b.CFrame,c.CFrame*bc.C1*(bc.C0*bc.Transform):Inverse())
frame(a.CFrame,b.CFrame*ab.C1*(ab.C0*ab.Transform):Inverse())
"###);
}

#[test]
fn unmodified_weldconstraint_branches_keep_their_relative_pose() {
    run(r###"
local function part(name,x)
 local p=Instance.new("Part",workspace);p.Name=name;p.CFrame=CFrame.new(x,0,0);return p
end
local a,b,c=part("A",0),part("B",4),part("C",8)
a.RootPriority=100
local ab=Instance.new("Motor6D",a);ab.Part0=a;ab.Part1=b;ab.C0=CFrame.new(4,0,0)
local bc=Instance.new("Motor6D",b);bc.Part0=b;bc.Part1=c;bc.C0=CFrame.new(4,0,0)

local accessory=part("Accessory",20);accessory.Massless=true
local w=Instance.new("WeldConstraint",c);w.Part0=c;w.Part1=accessory
local relative=c.CFrame:ToObjectSpace(accessory.CFrame)
ab.C0=CFrame.new(6,3,2)*CFrame.Angles(.5,.2,.4)
frame(accessory.CFrame,c.CFrame*relative)
"###);
}

#[test]
fn inactive_joint_edits_do_not_move_parts() {
    run(r###"
local function part(name,x)
 local p=Instance.new("Part",workspace);p.Name=name;p.CFrame=CFrame.new(x,0,0);return p
end
local a,b,c=part("A",0),part("B",4),part("C",8)
a.RootPriority=100
local ab=Instance.new("Motor6D",a);ab.Part0=a;ab.Part1=b;ab.C0=CFrame.new(4,0,0)
local bc=Instance.new("Motor6D",b);bc.Part0=b;bc.Part1=c;bc.C0=CFrame.new(4,0,0)

ab.Enabled=false;local aa,bb,cc=a.CFrame,b.CFrame,c.CFrame
ab.C0=CFrame.new(100,0,0)
frame(a.CFrame,aa);frame(b.CFrame,bb);frame(c.CFrame,cc)
assert(ab.C0.X==100)
"###);
}

#[test]
fn motor_targets_are_deferred_and_batched_in_parent_order() {
    run(r###"
local function part(name,x)
 local p=Instance.new("Part",workspace);p.Name=name;p.CFrame=CFrame.new(x,0,0);return p
end
local a,b,c=part("A",0),part("B",4),part("C",8)
a.RootPriority=100
local ab=Instance.new("Motor6D",a);ab.Part0=a;ab.Part1=b;ab.C0=CFrame.new(4,0,0)
local bc=Instance.new("Motor6D",b);bc.Part0=b;bc.Part1=c;bc.C0=CFrame.new(4,0,0)

local bb,cc=b.CFrame,c.CFrame
bc.Transform=CFrame.new(0,2,0)*CFrame.Angles(.2,.3,.4)
ab.Transform=CFrame.Angles(.3,.4,.5)
frame(b.CFrame,bb);frame(c.CFrame,cc)
task.wait(.001)
frame(a.CFrame,CFrame.identity)
frame(b.CFrame,a.CFrame*ab.C0*ab.Transform*ab.C1:Inverse())
frame(c.CFrame,b.CFrame*bc.C0*bc.Transform*bc.C1:Inverse())
"###);
}

#[test]
fn immediate_offsets_use_applied_transform_until_the_batch() {
    run(r###"
local function part(name,x)
 local p=Instance.new("Part",workspace);p.Name=name;p.CFrame=CFrame.new(x,0,0);return p
end
local a,b,c=part("A",0),part("B",4),part("C",8)
a.RootPriority=100
local ab=Instance.new("Motor6D",a);ab.Part0=a;ab.Part1=b;ab.C0=CFrame.new(4,0,0)
local bc=Instance.new("Motor6D",b);bc.Part0=b;bc.Part1=c;bc.C0=CFrame.new(4,0,0)

ab.Transform=CFrame.new(0,2,0)
ab.C0=CFrame.new(6,0,0)
vec(b.Position,Vector3.new(6,0,0))
task.wait(.001);vec(b.Position,Vector3.new(6,2,0));vec(c.Position,Vector3.new(10,2,0))
"###);
}

#[test]
fn connecting_preconfigured_joint_settles_rooted_branch() {
    run(r###"
local a=Instance.new("Part",workspace);a.Anchored=true;a.CFrame=CFrame.new(10,0,0)
local b=Instance.new("Part",workspace);b.CFrame=CFrame.new(100,0,0)
local j=Instance.new("Motor6D");j.C0=CFrame.new(3,2,0);j.C1=CFrame.new(1,0,0)
j.Transform=CFrame.new(0,0,5);j.Part0=a;j.Part1=b;j.Parent=a
task.wait(.001)
vec(a.Position,Vector3.new(10,0,0));vec(b.Position,Vector3.new(12,2,5))
"###);
}

#[test]
fn activation_without_transform_write_and_reenable() {
    run(r###"
local a=Instance.new("Part",workspace);a.Anchored=true
local b=Instance.new("Part",workspace);b.CFrame=CFrame.new(100,0,0)
local j=Instance.new("Weld",a);j.Enabled=false;j.C0=CFrame.new(4,0,0);j.Part0=a;j.Part1=b
j.Enabled=true;task.wait(.001);vec(b.Position,Vector3.new(4,0,0))
j.Enabled=false;b.CFrame=CFrame.new(80,0,0);j.Enabled=true
task.wait(.001);vec(b.Position,Vector3.new(4,0,0))
"###);
}

#[test]
fn parenting_whole_model_and_rewiring_endpoint() {
    run(r###"
local m=Instance.new("Model");local a=Instance.new("Part",m);a.Anchored=true
local b=Instance.new("Part",m);b.CFrame=CFrame.new(100,0,0)
local j=Instance.new("Weld",a);j.C0=CFrame.new(3,0,0);j.Part0=a;j.Part1=b
m.Parent=workspace;task.wait(.001);vec(b.Position,Vector3.new(3,0,0))
local c=Instance.new("Part",workspace);c.CFrame=CFrame.new(90,0,0)
j.Part1=c;task.wait(.001);vec(c.Position,Vector3.new(3,0,0));vec(b.Position,Vector3.new(3,0,0))
m.Parent=nil;a.CFrame=CFrame.new(20,0,0);m.Parent=workspace
task.wait(.001);vec(c.Position,Vector3.new(23,0,0))
"###);
}

#[test]
fn removing_conflicting_joint_activates_the_remaining_joint() {
    run(r###"
local a=Instance.new("Part",workspace);a.Anchored=true
local b=Instance.new("Part",workspace)
local x=Instance.new("Weld",a);x.C0=CFrame.new(3,0,0);x.Part0=a;x.Part1=b
local y=Instance.new("Weld",a);y.C0=CFrame.new(9,0,0);y.Part0=a;y.Part1=b
task.wait(.001);assert(x.Active and not y.Active);vec(b.Position,Vector3.new(3,0,0))
x:Destroy();task.wait(.001);assert(y.Active);vec(b.Position,Vector3.new(9,0,0))
"###);
}

#[test]
fn weldconstraint_activation_keeps_observed_offset() {
    run(r###"
local a=Instance.new("Part",workspace);a.Anchored=true
local b=Instance.new("Part",workspace);b.CFrame=CFrame.new(100,4,8)*CFrame.Angles(.2,.3,.4)
local before=b.CFrame;local j=Instance.new("WeldConstraint",a);j.Part0=a;j.Part1=b
task.wait(.001);frame(b.CFrame,before)
"###);
}

const RIG: &str = r###"
local function part(name,x)
 local p=Instance.new("Part",workspace);p.Name=name;p.CFrame=CFrame.new(x,0,0);return p
end
local a,b,c=part("A",0),part("B",4),part("C",8)
a.RootPriority=100
local ab=Instance.new("Motor6D",a);ab.Part0=a;ab.Part1=b;ab.C0=CFrame.new(4,0,0)
local bc=Instance.new("Motor6D",b);bc.Part0=b;bc.Part1=c;bc.C0=CFrame.new(4,0,0)
"###;

#[test]
fn phases_apply_presimulation_targets_before_postsimulation_and_waiters() {
    run(&format!(
        "{RIG}\n{}",
        r#"
        local service=game:GetService("RunService")
        local order={};local connections={}
        connections[1]=service.PreAnimation:Connect(function()
            table.insert(order,"animation");ab.Transform=CFrame.new(0,1,0)
            vec(b.Position,Vector3.new(4,0,0))
        end)
        connections[2]=service.PreSimulation:Connect(function()
            table.insert(order,"pre");ab.Transform=CFrame.new(0,3,0)
            vec(b.Position,Vector3.new(4,0,0))
        end)
        connections[3]=service.PostSimulation:Connect(function()
            table.insert(order,"post");vec(b.Position,Vector3.new(4,3,0));vec(c.Position,Vector3.new(8,3,0))
        end)
        connections[4]=service.Heartbeat:Connect(function()
            table.insert(order,"heartbeat")
            for _,connection in connections do connection:Disconnect() end
        end)
        task.wait()
        assert(table.concat(order,",")=="animation,pre,post,heartbeat",table.concat(order,","))
        vec(c.Position,Vector3.new(8,3,0))
    "#
    ));
}

#[test]
fn motor_retargeting_has_no_drift_and_coalesces_part_events() {
    run(&format!(
        "{RIG}\n{}",
        r#"
        local count=0
        b:GetPropertyChangedSignal("CFrame"):Connect(function()
            count+=1;frame(c.CFrame,b.CFrame*bc.C0*bc.Transform*bc.C1:Inverse())
        end)
        for i=1,12 do
            ab.Transform=CFrame.new(0,100,0)
            ab.Transform=CFrame.new(0,i,0)
            task.wait()
            vec(b.Position,Vector3.new(4,i,0));vec(c.Position,Vector3.new(8,i,0))
        end
        assert(count==12,tostring(count))
    "#
    ));
}

#[test]
fn position_and_angle_edits_rebase_joints_without_moving_neighbors() {
    run(&format!(
        "{RIG}\n{}",
        r#"
        local aa,cc=a.CFrame,c.CFrame
        b.Position=Vector3.new(5,2,3)
        frame(a.CFrame,aa);frame(c.CFrame,cc)
        frame(a.CFrame*ab.C0,b.CFrame*ab.C1)
        frame(b.CFrame*bc.C0,c.CFrame*bc.C1)
        b.Orientation=Vector3.new(10,20,30)
        frame(a.CFrame,aa);frame(c.CFrame,cc)
        frame(a.CFrame*ab.C0,b.CFrame*ab.C1)
        frame(b.CFrame*bc.C0,c.CFrame*bc.C1)
        local rel=b.CFrame:ToObjectSpace(c.CFrame)
        ab.C0=CFrame.new(6,2,1)*CFrame.Angles(.2,.1,.3)
        frame(c.CFrame,b.CFrame*rel)
    "#
    ));
}

fn pose(x: f64, y: f64, z: f64) -> Value {
    json!({"$cframe":{"position":[x,y,z],"rotation":[1,0,0,0,1,0,0,0,1]}})
}
fn snapshot() -> Value {
    let properties = |x, priority| json!({"CFrame":pose(x,0.,0.),"Anchored":false,"Massless":false,"RootPriority":priority,"Size":{"$vector3":[2,2,2]}});
    let joint = |a, b| json!({"Enabled":true,"Part0":{"$ref":a},"Part1":{"$ref":b},"C0":pose(4.,0.,0.),"C1":pose(0.,0.,0.),"Transform":pose(0.,0.,0.)});
    json!({"version":1,"sequence":1,"services":{"Workspace":"workspace"},"nodes":[
        {"id":"workspace","class_name":"Workspace","name":"Workspace","properties":{}},
        {"id":"a","class_name":"Part","name":"A","parent":"workspace","properties":properties(0.,100)},
        {"id":"b","class_name":"Part","name":"B","parent":"workspace","properties":properties(4.,0)},
        {"id":"c","class_name":"Part","name":"C","parent":"workspace","properties":properties(8.,0)},
        {"id":"ab","class_name":"Motor6D","name":"AB","parent":"a","properties":joint("a","b")},
        {"id":"bc","class_name":"Motor6D","name":"BC","parent":"b","properties":joint("b","c")}
    ]})
}
fn update(r: &mut Runtime, sequence: &mut u64, mut actions: Vec<Value>, errors: bool) -> Value {
    for action in &mut actions {
        *sequence += 1;
        action["sequence"] = json!(*sequence);
    }
    let reply=r.call(json!({"op":"update","delta":{"version":1,"delta":true,"sequence":*sequence,"actions":actions}})).unwrap();
    assert_eq!(reply["ok"], true, "{reply}");
    assert_eq!(
        reply["joint_pose_errors"]
            .as_array()
            .is_some_and(|v| !v.is_empty()),
        errors,
        "{reply}"
    );
    assert!(
        reply["mutations"].as_array().is_none_or(|v| !v
            .iter()
            .any(|m| matches!(m["id"].as_str(), Some("a" | "b" | "c" | "ab" | "bc")))),
        "receipt echoed authored edits: {reply}"
    );
    if !errors {
        check(&reply);
    }
    reply
}

#[test]
fn received_offsets_are_atomic_and_have_received_origin_without_echo() {
    let mut r = Runtime::new(snapshot(), 32).unwrap();
    let mut seq = 1;
    execute(
        &mut r,
        r#"
        getgenv().count=0
        workspace.B:GetPropertyChangedSignal("CFrame"):Connect(function()
            count+=1;vec(workspace.C.Position,Vector3.new(12,2,5))
        end)
    "#,
    );
    update(
        &mut r,
        &mut seq,
        vec![
            json!({"kind":"set","id":"bc","fields":{"C0":pose(6.,0.,5.)}}),
            json!({"kind":"set","id":"ab","fields":{"C0":pose(6.,2.,0.)}}),
        ],
        false,
    );
    execute(
        &mut r,
        r#"
        vec(workspace.B.Position,Vector3.new(6,2,0));vec(workspace.C.Position,Vector3.new(12,2,5))
        assert(count==1,tostring(count))
        assert(client.propertyOrigin(workspace.B,"CFrame")=="derived_received")
        local copy=workspace.B:Clone();vec(copy.Position,workspace.B.Position)
        frame(copy.CFrame,workspace.B.CFrame)
    "#,
    );
}

#[test]
fn received_part_pose_wins_in_either_order_and_cuts_inherited_motion() {
    for reverse in [false, true] {
        let mut r = Runtime::new(snapshot(), 32).unwrap();
        let mut seq = 1;
        let mut actions = vec![
            json!({"kind":"set","id":"ab","fields":{"C0":pose(30.,0.,0.)}}),
            json!({"kind":"set","id":"bc","fields":{"C0":pose(6.,0.,0.)}}),
            json!({"kind":"set","id":"b","fields":{"CFrame":pose(40.,0.,0.)}}),
        ];
        if reverse {
            actions.reverse();
        }
        update(&mut r, &mut seq, actions, false);
        execute(
            &mut r,
            r#"
            task.wait();vec(workspace.B.Position,Vector3.new(40,0,0));vec(workspace.C.Position,Vector3.new(46,0,0))
            assert(client.propertyOrigin(workspace.B,"CFrame")=="received")
        "#,
        );
    }
}

#[test]
fn bad_received_child_blocks_entire_batch_and_repair_retries_targets() {
    let mut r = Runtime::new(snapshot(), 32).unwrap();
    let mut seq = 1;
    update(
        &mut r,
        &mut seq,
        vec![
            json!({"kind":"set","id":"ab","fields":{"Transform":pose(0.,3.,0.)}}),
            json!({"kind":"set","id":"bc","fields":{"Transform":pose(0.,2.,0.),"C1":{"$cframe":{"position":[0,0,0]}}}}),
        ],
        true,
    );
    execute(
        &mut r,
        "vec(workspace.B.Position,Vector3.new(4,0,0));vec(workspace.C.Position,Vector3.new(8,0,0))",
    );
    update(
        &mut r,
        &mut seq,
        vec![
            json!({"kind":"set","id":"ab","fields":{"C1":pose(0.,0.,0.)}}),
            json!({"kind":"set","id":"bc","fields":{"C1":pose(0.,0.,0.)}}),
        ],
        false,
    );
    execute(
        &mut r,
        "vec(workspace.B.Position,Vector3.new(4,3,0));vec(workspace.C.Position,Vector3.new(8,5,0))",
    );
}

#[test]
fn initial_snapshot_and_received_create_keep_authoritative_poses() {
    let mut data = snapshot();
    data["nodes"][2]["properties"]["CFrame"] = pose(40., 0., 0.);
    let mut r = Runtime::new(data.clone(), 32).unwrap();
    execute(
        &mut r,
        "task.wait();vec(workspace.B.Position,Vector3.new(40,0,0))",
    );
    let mut r = runtime();
    let mut seq = 1;
    let nodes = data["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .skip(1)
        .rev()
        .map(|n| json!({"kind":"create","id":n["id"],"node":n}))
        .collect();
    update(&mut r, &mut seq, nodes, false);
    execute(&mut r,"task.wait();vec(workspace.B.Position,Vector3.new(40,0,0));vec(workspace.C.Position,Vector3.new(8,0,0))");
}

#[test]
fn received_create_can_derive_missing_child_frames_in_any_creation_order() {
    let mut data = snapshot();
    for i in [2, 3] {
        data["nodes"][i]["properties"]
            .as_object_mut()
            .unwrap()
            .remove("CFrame");
    }
    let mut r = runtime();
    let mut seq = 1;
    let nodes = data["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .skip(1)
        .rev()
        .map(|n| json!({"kind":"create","id":n["id"],"node":n}))
        .collect();
    update(&mut r, &mut seq, nodes, false);
    execute(
        &mut r,
        "vec(workspace.B.Position,Vector3.new(4,0,0));vec(workspace.C.Position,Vector3.new(8,0,0))",
    );
}

#[test]
fn unknown_root_metadata_rejects_immediate_offset_without_partial_writes() {
    let mut data = snapshot();
    data["nodes"][2]["properties"]
        .as_object_mut()
        .unwrap()
        .remove("Massless");
    let mut r = Runtime::new(data, 32).unwrap();
    execute(
        &mut r,
        r#"
        assert(not pcall(function()workspace.A.AB.C0=CFrame.new(20,0,0)end))
        vec(workspace.B.Position,Vector3.new(4,0,0));assert(workspace.A.AB.C0.X==4)
    "#,
    );
}
