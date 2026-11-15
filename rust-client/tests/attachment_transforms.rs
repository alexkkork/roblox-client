#![cfg(feature = "rust-host")]
use headless_luau::runtime_owner::Runtime;
use serde_json::{json, Value};
use std::time::{Duration, Instant};
fn runtime() -> Runtime {
    Runtime::new(json!({"version":1,"sequence":1,"services":{"Workspace":"workspace"},"nodes":[{"id":"workspace","class_name":"Workspace","name":"Workspace","parent":null,"properties":{}}]}),32).unwrap()
}
fn execute(r: &mut Runtime, source: &str) -> Value {
    let started = r
        .call(json!({"op":"execute","source":format!("{HELPERS}\n{source}"),"timeout_ms":2000}))
        .unwrap();
    let id = started["job_id"]
        .as_u64()
        .unwrap_or_else(|| panic!("{started}"));
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let result = r.status(id).unwrap();
        assert_eq!(result["ok"], true, "{result}");
        if result["completed"] == true {
            return result;
        }
        assert!(Instant::now() < until, "{result}");
        r.poll().unwrap();
        std::thread::sleep(Duration::from_millis(1));
    }
}
const HELPERS: &str = r#"
local function near(a,b,e) assert(math.abs(a-b)<(e or .0001),tostring(a).." ~= "..tostring(b)) end
local function vec(a,b) near(a.X,b.X);near(a.Y,b.Y);near(a.Z,b.Z) end
local function frame(a,b) local av,bv={a:GetComponents()},{b:GetComponents()};for i=1,12 do near(av[i],bv[i]) end end
"#;
fn run(source: &str) {
    execute(&mut runtime(), source);
}
#[test]
fn packaged_attachment_smoke_runs_with_real_luau() {
    let result = execute(
        &mut runtime(),
        include_str!("../../examples/attachment-transforms-smoke.luau"),
    );
    assert_eq!(
        result["values"][0]["passed"].as_f64(),
        Some(18.),
        "{result}"
    );
}
#[test]
fn deep_received_ancestry_uses_iterative_transform_walk() {
    let mut nodes = vec![
        json!({"id":"workspace","class_name":"Workspace","name":"Workspace","parent":null,"properties":{"Point":{"$ref":"point"}}}),
        json!({"id":"part","class_name":"Part","name":"P","parent":"workspace","properties":{"CFrame":pose([10.,0.,0.])}}),
    ];
    let mut parent = "part".to_owned();
    for i in 0..2048 {
        let id = format!("folder-{i}");
        nodes.push(
            json!({"id":id,"class_name":"Folder","name":"F","parent":parent,"properties":{}}),
        );
        parent = id;
    }
    nodes.push(json!({"id":"point","class_name":"Attachment","name":"A","parent":parent,"properties":{"CFrame":pose([1.,2.,3.])}}));
    let mut r = Runtime::new(
        json!({"version":1,"sequence":1,"services":{"Workspace":"workspace"},"nodes":nodes}),
        32,
    )
    .unwrap();
    execute(&mut r,"vec(workspace.Point.WorldPosition,Vector3.new(11,2,3));workspace.Point.WorldPosition=Vector3.new(20,30,40);vec(workspace.Point.Position,Vector3.new(10,30,40))");
}
fn pose(p: [f64; 3]) -> Value {
    json!({"$cframe":{"position":p,"rotation":[1,0,0,0,1,0,0,0,1]}})
}
fn received(properties: Value, parent: &str) -> Runtime {
    Runtime::new(json!({"version":1,"sequence":1,"services":{"Workspace":"workspace"},"nodes":[
        {"id":"workspace","class_name":"Workspace","name":"Workspace","parent":null,"properties":{"Point":{"$ref":"point"}}},
        {"id":"part","class_name":"Part","name":"P","parent":"workspace","properties":{"CFrame":pose([10.,0.,0.])}},
        {"id":"point","class_name":"Attachment","name":"A","parent":parent,"properties":properties}
    ]}),32).unwrap()
}
fn update(r: &mut Runtime, sequence: u64, id: &str, fields: Value) -> Value {
    r.call(
        json!({"op":"update","delta":{"version":1,"delta":true,"sequence":sequence,
        "actions":[{"kind":"set","sequence":sequence,"id":id,"fields":fields}]}}),
    )
    .unwrap()
}

#[test]
fn unknown_rotation_and_parent_stay_unknown_until_explicitly_authored() {
    let mut r = received(
        json!({"CFrame":{"$cframe":{"position":[1,2,3],"rotation_id":17}},
        "WorldCFrame":pose([99.,99.,99.]),"Axis":{"$vector3":[1,0,0]}}),
        "part",
    );
    execute(
        &mut r,
        r#"
        local a=workspace.Point
        vec(a.Position,Vector3.new(1,2,3));vec(a.WorldPosition,Vector3.new(11,2,3))
        assert(not pcall(function()return a.WorldCFrame end))
        assert(not pcall(function()return a.Axis end))
        assert(client.propertyOrigin(a,"WorldCFrame")=="unavailable")
        local clone=a:Clone();vec(clone.Position,a.Position)
        assert(not pcall(function()return clone.Orientation end))
        a.WorldPosition=Vector3.new(14,5,6);vec(a.Position,Vector3.new(4,5,6))
        assert(not pcall(function()return a.Orientation end))
        a.WorldOrientation=Vector3.new(0,90,0)
        frame(a.WorldCFrame,CFrame.new(14,5,6)*CFrame.Angles(0,math.pi/2,0))
        assert(client.propertyOrigin(a,"WorldCFrame")=="local")
    "#,
    );
    let mut r = received(json!({"CFrame":pose([1.,2.,3.])}), "missing-parent");
    execute(
        &mut r,
        r#"
        local a=workspace.Point;vec(a.Position,Vector3.new(1,2,3))
        assert(not pcall(function()return a.WorldPosition end))
        assert(not pcall(function()a.WorldPosition=Vector3.new(5,6,7)end))
        vec(a.Position,Vector3.new(1,2,3))
    "#,
    );
}

#[test]
fn snapshot_components_and_received_axis_history_survive_clone() {
    let mut r = received(
        json!({"Position":{"$vector3":[1,2,3]},"Orientation":{"$vector3":[20,30,40]}}),
        "part",
    );
    execute(
        &mut r,
        r#"
        getgenv().expected=Instance.new("Attachment")
        expected.CFrame=CFrame.new(1,2,3)*CFrame.fromOrientation(math.rad(20),math.rad(30),math.rad(40))
        frame(workspace.Point.CFrame,expected.CFrame)
    "#,
    );
    let steps = [
        ("Axis", [1, 2, 3]),
        ("SecondaryAxis", [3, -1, 1]),
        ("Axis", [1, -2, 1]),
        ("SecondaryAxis", [0, 1, 2]),
        ("Axis", [1, 0, 0]),
    ];
    for (i, (key, v)) in steps.into_iter().enumerate() {
        assert_eq!(
            update(&mut r, i as u64 + 2, "point", json!({key:{"$vector3":v}}))["ok"],
            true
        );
        execute(&mut r,&format!("expected.{key}=Vector3.new({},{},{});frame(workspace.Point.CFrame,expected.CFrame);local c=workspace.Point:Clone();frame(c.CFrame,expected.CFrame)",v[0],v[1],v[2]));
    }
    assert_eq!(
        update(&mut r, 10, "point", json!({"CFrame":pose([8.,9.,10.])}))["ok"],
        true
    );
    execute(&mut r, "frame(workspace.Point.CFrame,CFrame.new(8,9,10))");
}

#[test]
fn received_pose_updates_emit_each_derived_signal_once() {
    let mut r = received(json!({"CFrame":pose([1.,2.,3.])}), "part");
    execute(
        &mut r,
        r#"
        getgenv().calls=0;getgenv().positionCalls=0;getgenv().a=workspace.Point
        a:GetPropertyChangedSignal("WorldPosition"):Connect(function()
            calls+=1;vec(a.WorldPosition,(workspace.P.CFrame*a.CFrame).Position)
        end)
        a:GetPropertyChangedSignal("Position"):Connect(function()positionCalls+=1 end)
    "#,
    );
    assert_eq!(
        update(&mut r, 2, "part", json!({"CFrame":pose([20.,0.,0.])}))["ok"],
        true
    );
    execute(
        &mut r,
        "assert(calls==1 and positionCalls==0);vec(a.WorldPosition,Vector3.new(21,2,3))",
    );
    assert_eq!(
        update(
            &mut r,
            3,
            "point",
            json!({"CFrame":pose([4.,5.,6.]),"WorldPosition":{"$vector3":[99,99,99]}})
        )["ok"],
        true
    );
    execute(&mut r,"assert(calls==2 and positionCalls==1);vec(a.WorldPosition,Vector3.new(24,5,6));assert(client.propertyOrigin(a,'WorldCFrame')=='derived_received')");
    assert_eq!(
        update(&mut r, 4, "part", json!({"CFrame":pose([20.,0.,0.])}))["ok"],
        true
    );
    execute(&mut r, "assert(calls==2)");
    execute(&mut r, "a.CFrame=CFrame.new(90,80,70)");
    assert_eq!(
        update(&mut r, 5, "point", json!({"Position":{"$vector3":[1,3,5]}}))["ok"],
        true
    );
    execute(
        &mut r,
        "frame(a.CFrame,CFrame.new(1,3,5));assert(calls==4 and positionCalls==3)",
    );
}

#[test]
fn assigning_received_pose_to_itself_does_not_emit_a_change() {
    let mut r = received(
        json!({"CFrame":{"$cframe":{"position":[1,2,3],"rotation":[1,0,0,0,1,0,0,0,1]}}}),
        "part",
    );
    execute(
        &mut r,
        r#"
        local a=workspace.Point;local calls=0
        a.Changed:Connect(function()calls+=1 end)
        a.CFrame=a.CFrame;task.wait();assert(calls==0)
    "#,
    );
}

#[test]
fn invalid_received_axis_bundle_rolls_back_every_action() {
    let mut r = received(json!({"CFrame":pose([1.,2.,3.])}), "part");
    let result=r.call(json!({"op":"update","delta":{"version":1,"delta":true,"sequence":3,"actions":[
        {"sequence":2,"kind":"set","id":"part","fields":{"CFrame":pose([80.,0.,0.])}},
        {"sequence":3,"kind":"set","id":"point","fields":{"Position":{"$vector3":[9,9,9]},"Axis":{"$vector3":[0,0,0]}}}
    ]}}));
    assert!(result.is_err() || result.unwrap()["ok"] == false);
    execute(
        &mut r,
        "vec(workspace.Point.WorldPosition,Vector3.new(11,2,3))",
    );
    let result=r.call(json!({"op":"update","delta":{"version":1,"delta":true,"sequence":3,"actions":[
        {"sequence":3,"kind":"set","id":"point","fields":{"Axis":{"$vector3":[1,0,0]},"SecondaryAxis":{"$vector3":[1,0,0]}}}
    ]}}));
    assert!(result.is_err() || result.unwrap()["ok"] == false);
    execute(&mut r, "frame(workspace.Point.CFrame,CFrame.new(1,2,3))");
}

#[test]
fn parent_and_bone_changes_notify_after_complete_pose_without_duplicates() {
    run(r#"
        local p=Instance.new("Part",workspace);p.CFrame=CFrame.new(10,0,0)
        local a=Instance.new("Attachment",p);a.Position=Vector3.new(1,2,3)
        local keys={};a.Changed:Connect(function(key)keys[key]=(keys[key] or 0)+1 end)
        a.WorldPosition=Vector3.new(15,6,7);task.wait()
        assert(keys.WorldPosition==1 and keys.Position==1)
        keys={};a.Parent=workspace;task.wait()
        assert(keys.WorldPosition==1 and keys.Parent==1);vec(a.WorldPosition,a.Position)
        keys={};a.Parent=workspace;task.wait();assert(next(keys)==nil)
        local b=Instance.new("Bone",p);local c=Instance.new("Bone",b);c.Position=Vector3.new(1,2,3)
        local rest,posed=0,0
        c:GetPropertyChangedSignal("WorldCFrame"):Connect(function()rest+=1 end)
        c:GetPropertyChangedSignal("TransformedWorldCFrame"):Connect(function()posed+=1 end)
        b.Transform=CFrame.new(5,0,0);task.wait()
        assert(rest==0 and posed==1);vec(c.TransformedWorldCFrame.Position,Vector3.new(16,2,3))
    "#);
}

#[test]
fn listeners_start_at_connect_time_and_disabled_connections_do_not_replay() {
    run(r#"
        local p=Instance.new("Part",workspace);local a=Instance.new("Attachment",p)
        local signal=a:GetPropertyChangedSignal("WorldPosition");p.Position=Vector3.new(2,0,0)
        local count,once,waited=0,0,false
        local connection=signal:Connect(function()count+=1 end)
        signal:Once(function()once+=1 end)
        task.spawn(function()signal:Wait();waited=true end);task.wait()
        assert(count==0 and not waited)
        connection:Disable();p.Position=Vector3.new(3,0,0);task.wait()
        assert(count==0 and once==1 and waited)
        connection:Enable();task.wait();assert(count==0)
        p.Position=Vector3.new(4,0,0);task.wait();assert(count==1 and once==1)
        connection:Disconnect();p.Position=Vector3.new(5,0,0);task.wait();assert(count==1)
        a:Destroy();p.Position=Vector3.new(6,0,0)
    "#);
}

#[test]
fn pivot_scale_and_reparent_refresh_attachment_observers() {
    run(r#"
        local m=Instance.new("Model",workspace);m.WorldPivot=CFrame.new()
        local p=Instance.new("Part",m);p.CFrame=CFrame.new(2,0,0)
        local a=Instance.new("Attachment",p);a.Position=Vector3.new(1,0,0)
        local count=0;a:GetPropertyChangedSignal("WorldPosition"):Connect(function()count+=1 end)
        m:ScaleTo(2);task.wait();assert(count==1);vec(a.WorldPosition,Vector3.new(6,0,0))
        m:PivotTo(CFrame.new(10,0,0));task.wait();assert(count==2);vec(a.WorldPosition,Vector3.new(16,0,0))
        a.Parent=m;task.wait();assert(count==3);vec(a.WorldPosition,Vector3.new(12,0,0))
    "#);
}

#[test]
fn bad_inputs_readonly_fields_and_deep_hierarchies_are_safe() {
    run(r#"
        local a=Instance.new("Bone");a.CFrame=CFrame.new(1,2,3)
        for key,v in pairs({Axis=Vector3.zero,SecondaryAxis=Vector3.xAxis,Position=Vector3.new(0/0,0,0),WorldPosition={},WorldRotation=Vector3.zero,TransformedCFrame=CFrame.new(),TransformedWorldCFrame=CFrame.new(),Transform=Vector3.zero}) do
            assert(not pcall(function()a[key]=v end),key);frame(a.CFrame,CFrame.new(1,2,3))
        end
        local p=Instance.new("Part",workspace);p.CFrame=CFrame.new(10,0,0)
        local parent=p
        for i=1,256 do parent=Instance.new("Folder",parent) end
        a.Parent=parent;vec(a.WorldPosition,Vector3.new(11,2,3))
    "#);
}
#[test]
fn local_pose_aliases_and_rotation_orders() {
    run(r###"local a=Instance.new("Attachment")
a.Position=Vector3.new(1,2,3);a.Orientation=Vector3.new(20,30,40)
frame(a.CFrame,CFrame.new(1,2,3)*CFrame.fromOrientation(math.rad(20),math.rad(30),math.rad(40)))
a.Rotation=Vector3.new(-12,35,16)
frame(a.CFrame,CFrame.new(1,2,3)*CFrame.Angles(math.rad(-12),math.rad(35),math.rad(16)))
frame(a.WorldCFrame,a.CFrame);vec(a.WorldPosition,a.Position)
vec(a.Axis,a.CFrame.RightVector);vec(a.SecondaryAxis,a.CFrame.UpVector)
vec(a:GetAxis(),a.Axis);vec(a:GetSecondaryAxis(),a.SecondaryAxis)
local x,y,z=a.CFrame:ToOrientation();vec(a.Orientation,Vector3.new(math.deg(x),math.deg(y),math.deg(z)))"###);
}

#[test]
fn nested_attachments_follow_nearest_transform_through_folders() {
    run(r###"local p=Instance.new("Part",workspace)
p.CFrame=CFrame.new(10,20,30)*CFrame.Angles(.2,.4,.6)
p.PivotOffset=CFrame.new(100,0,0)
local a=Instance.new("Attachment",Instance.new("Folder",p));a.CFrame=CFrame.new(1,2,3)*CFrame.Angles(.3,.1,.7)
local b=Instance.new("Attachment",Instance.new("Folder",a));b.CFrame=CFrame.new(-4,5,6)*CFrame.Angles(.5,-.2,.8)
frame(a.WorldCFrame,p.CFrame*a.CFrame);frame(b.WorldCFrame,p.CFrame*a.CFrame*b.CFrame)
vec(b.WorldAxis,b.WorldCFrame.RightVector);vec(b.WorldSecondaryAxis,b.WorldCFrame.UpVector)
p.CFrame=CFrame.new(-3,-6,-9)*CFrame.Angles(.7,.2,.1)
frame(b.WorldCFrame,p.CFrame*a.CFrame*b.CFrame)
b.Parent=workspace;frame(b.WorldCFrame,b.CFrame)
b.Parent=nil;frame(b.WorldCFrame,b.CFrame)"###);
}

#[test]
fn world_setters_compute_local_pose_and_preserve_other_components() {
    run(
        r###"local p=Instance.new("Part");p.CFrame=CFrame.new(10,20,30)*CFrame.Angles(.2,.7,-.4)
local a=Instance.new("Attachment",p)
local target=CFrame.new(-5,12,8)*CFrame.Angles(.8,-.3,.2)
a.WorldCFrame=target;frame(a.WorldCFrame,target);frame(a.CFrame,p.CFrame:ToObjectSpace(target))
local rot=a.CFrame.Rotation;a.WorldPosition=Vector3.new(40,50,60)
vec(a.WorldPosition,Vector3.new(40,50,60));frame(a.CFrame.Rotation,rot)
a.WorldOrientation=Vector3.new(15,25,35)
frame(a.WorldCFrame,CFrame.new(40,50,60)*CFrame.fromOrientation(math.rad(15),math.rad(25),math.rad(35)))
assert(not pcall(function()a.WorldRotation=Vector3.zero end))"###,
    );
}

#[test]
fn axes_normalize_and_reproject_and_deprecated_methods_work() {
    run(
        r###"local a=Instance.new("Attachment");a.Position=Vector3.new(1,2,3)
a.Axis=Vector3.new(2,1,0);near(a.Axis.Magnitude,1);near(a.SecondaryAxis.Magnitude,1);near(a.Axis:Dot(a.SecondaryAxis),0)
vec(a.Axis,Vector3.new(2,1,0).Unit)
assert(select('#',a:SetSecondaryAxis(Vector3.new(1,3,2)))==0)
near(a.Axis:Dot(a.SecondaryAxis),0);near(a.SecondaryAxis.Magnitude,1)
frame(a.CFrame.Rotation,CFrame.fromMatrix(Vector3.zero,a.Axis,a.SecondaryAxis,a.Axis:Cross(a.SecondaryAxis)))
vec(a.Position,Vector3.new(1,2,3))
a.CFrame=CFrame.identity;assert(select('#',a:SetAxis(Vector3.yAxis))==0)
vec(a.Axis,Vector3.yAxis);near(a.Axis:Dot(a.SecondaryAxis),0)
assert(not pcall(function()a.Axis=Vector3.zero end))
assert(not pcall(function()a.SecondaryAxis=a.Axis end))
local get=a.GetAxis;assert(not pcall(get,Instance.new("Folder")))"###,
    );
}

#[test]
fn world_axes_transform_directions_without_translation() {
    run(
        r###"local p=Instance.new("Part");p.CFrame=CFrame.new(100,200,300)*CFrame.Angles(.3,.6,.9)
local a=Instance.new("Attachment",p);a.Position=Vector3.new(4,5,6)
local original=a.WorldPosition;local axis=Vector3.new(1,2,3).Unit
a.WorldAxis=axis;vec(a.WorldAxis,axis)
a.WorldSecondaryAxis=Vector3.new(-2,1,4)
near(a.WorldAxis:Dot(a.WorldSecondaryAxis),0);near(a.WorldSecondaryAxis.Magnitude,1)
vec(a.WorldPosition,original);vec(a.WorldAxis,p.CFrame:VectorToWorldSpace(a.Axis))"###,
    );
}

#[test]
fn model_pivots_and_camera_ancestors() {
    run(
        r###"local m=Instance.new("Model");m.WorldPivot=CFrame.new(10,20,30)*CFrame.Angles(.3,.4,.5)
local a=Instance.new("Attachment",m);a.CFrame=CFrame.new(1,2,3)
frame(a.WorldCFrame,m:GetPivot()*a.CFrame)
m:PivotTo(CFrame.new(40,50,60));frame(a.WorldCFrame,m:GetPivot()*a.CFrame)
local c=Instance.new("Camera");c.CFrame=CFrame.new(4,5,6)*CFrame.Angles(.1,.2,.3)
a.Parent=c;frame(a.WorldCFrame,c.CFrame*a.CFrame)"###,
    );
}

#[test]
fn bone_reference_and_animated_transforms_are_distinct() {
    run(
        r###"local p=Instance.new("MeshPart");p.CFrame=CFrame.new(10,20,30)*CFrame.Angles(.1,.2,.3)
local a=Instance.new("Bone",p);a.CFrame=CFrame.new(1,2,3);a.Transform=CFrame.Angles(.4,.5,.6)
local b=Instance.new("Bone",a);b.CFrame=CFrame.new(4,5,6);b.Transform=CFrame.new(0,2,0)*CFrame.Angles(.2,.1,.4)
frame(a.TransformedCFrame,a.CFrame*a.Transform)
frame(b.WorldCFrame,p.CFrame*a.CFrame*b.CFrame)
frame(b.TransformedWorldCFrame,p.CFrame*a.CFrame*a.Transform*b.CFrame*b.Transform)
assert(not pcall(function()b.TransformedWorldCFrame=CFrame.identity end))
assert(not pcall(function()b.TransformedCFrame=CFrame.identity end))"###,
    );
}

#[test]
fn get_constraints_returns_deduplicated_snapshot_and_tracks_disconnect() {
    run(
        r###"local a,b=Instance.new("Attachment"),Instance.new("Attachment")
local one=Instance.new("HingeConstraint");one.Attachment0=a;one.Attachment1=a
local two=Instance.new("BallSocketConstraint");two.Attachment0=b;two.Attachment1=a
local other=Instance.new("HingeConstraint");other.Attachment0=b
local list=a:GetConstraints();assert(#list==2 and list[1]==one and list[2]==two)
one.Attachment0=b;one.Attachment1=nil;two:Destroy()
assert(#a:GetConstraints()==0 and #list==2)
local get=a.GetConstraints;assert(not pcall(get,Instance.new("Folder")))"###,
    );
}

#[test]
fn clone_rebinds_parent_and_constraint_references() {
    run(
        r###"local m=Instance.new("Model");local p=Instance.new("Part",m);p.CFrame=CFrame.new(10,20,30)
local a=Instance.new("Attachment",p);a.CFrame=CFrame.new(1,2,3)
local c=Instance.new("HingeConstraint",m);c.Attachment0=a
local copy=m:Clone();local ca=copy.Part.Attachment
copy.Part.Position=Vector3.new(40,50,60)
vec(ca.WorldPosition,Vector3.new(41,52,63));vec(a.WorldPosition,Vector3.new(11,22,33))
assert(ca:GetConstraints()[1]==copy.HingeConstraint and #ca:GetConstraints()==1)"###,
    );
}
