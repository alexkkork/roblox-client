#![cfg(feature = "rust-host")]
use headless_luau::runtime_owner::Runtime;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

fn runtime() -> Runtime {
    Runtime::new(json!({"version":1,"sequence":1,"services":{"Workspace":"workspace"},"nodes":[
    {"id":"workspace","class_name":"Workspace","name":"Workspace","parent":null,"properties":{}}
]}),32).unwrap()
}
fn execute(r: &mut Runtime, source: &str) -> Value {
    let start = r
        .call(json!({"op":"execute","source":source,"timeout_ms":2000}))
        .unwrap();
    let id = start["job_id"]
        .as_u64()
        .unwrap_or_else(|| panic!("{start}"));
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
const SETUP: &str = r#"
local m=Instance.new("Model",workspace)
local function part(parent,cf,size)
    local p=Instance.new("Part",parent or m); p.Anchored=true
    p.CFrame=cf or CFrame.new();p.Size=size or Vector3.new(2,2,2);return p
end
local function near(a,b,eps) assert(math.abs(a-b)<(eps or .0001),tostring(a).." ~= "..tostring(b)) end
local function same(a,b,eps)
    local av,bv={a:GetComponents()},{b:GetComponents()}
    for i=1,12 do near(av[i],bv[i],eps) end
end
"#;
fn run(source: &str) {
    execute(&mut runtime(), &format!("{SETUP}\n{source}"));
}

#[test]
fn part_pivot_offset_and_target_are_composed_in_the_correct_order() {
    run(r#"
    local p=part(nil,CFrame.new(3,4,5)*CFrame.Angles(.2,.3,.4))
    p.PivotOffset=CFrame.new(2,-1,3)*CFrame.Angles(-.4,.1,.6)
    same(p:GetPivot(),p.CFrame*p.PivotOffset)
    local target=CFrame.new(30,20,10)*CFrame.Angles(.7,-.3,.1)
    p:PivotTo(target)
    same(p:GetPivot(),target);same(p.CFrame,target*p.PivotOffset:Inverse())
    assert(p.Position==p.CFrame.Position)
"#);
}

#[test]
fn untouched_models_follow_bounds_until_an_explicit_pivot_is_set() {
    run(r#"
    local a=part(nil,CFrame.new(0,0,0),Vector3.new(2,2,2))
    local b=part(nil,CFrame.new(10,0,0),Vector3.new(4,6,8))
    local cf,size=m:GetBoundingBox()
    same(cf,CFrame.new(5.5,0,0));assert(size==Vector3.new(13,6,8))
    assert(m:GetExtentsSize()==size);same(m:GetPivot(),cf);same(m.WorldPivot,cf)
    b.CFrame=CFrame.new(20,0,0);near(m:GetPivot().X,10.5)
    m.WorldPivot=CFrame.new()
    b.CFrame=CFrame.new(30,0,0);same(m:GetPivot(),CFrame.new())
    near(b.Position.X,30);near(m:GetBoundingBox().X,15.5)
"#);
}

#[test]
fn pivot_to_freezes_the_model_pivot_and_moves_all_parts() {
    run(r#"
    local a=part(nil,CFrame.new(-2,0,0));local b=part(nil,CFrame.new(2,0,0))
    local target=CFrame.new(10,20,30)*CFrame.Angles(.1,.2,.3)
    m:PivotTo(target);same(m:GetPivot(),target)
    same(a.CFrame,target*CFrame.new(-2,0,0));same(b.CFrame,target*CFrame.new(2,0,0))
    b.CFrame=CFrame.new(50,0,0);same(m:GetPivot(),target)
"#);
}

#[test]
fn primary_part_pivot_offset_controls_model_orientation_and_bounds() {
    run(r#"
    local p=part(nil,CFrame.new(10,2,0),Vector3.new(4,2,6))
    p.PivotOffset=CFrame.new(1,2,3)*CFrame.Angles(0,0,math.pi/2);m.PrimaryPart=p
    m.WorldPivot=CFrame.new(90,0,0)
    same(m:GetPivot(),p:GetPivot())
    local cf,size=m:GetBoundingBox();same(cf,p.CFrame*CFrame.Angles(0,0,math.pi/2))
    near(size.X,2);near(size.Y,4);near(size.Z,6)
    local target=CFrame.new(-5,2,7)*CFrame.Angles(.5,.1,-.2)
    m:PivotTo(target);same(m:GetPivot(),target)
    same(m:GetPrimaryPartCFrame(),p.CFrame)
    m:SetPrimaryPartCFrame(CFrame.new(42,0,0));same(p.CFrame,CFrame.new(42,0,0))
    same(m:GetPivot(),p.CFrame*p.PivotOffset)
"#);
}

#[test]
fn nested_models_parts_and_cameras_receive_the_same_transform() {
    run(r#"
    local nested=Instance.new("Model",m)
    local p=part(nested,CFrame.new(5,0,0));local q=part(p,CFrame.new(8,0,0))
    local camera=Instance.new("Camera",nested);camera.CFrame=CFrame.new(2,3,4)
    nested.WorldPivot=CFrame.new(6,1,0);m.WorldPivot=CFrame.new()
    local target=CFrame.new(30,10,2)*CFrame.Angles(.1,.2,.3)
    m:PivotTo(target)
    same(p.CFrame,target*CFrame.new(5,0,0));same(q.CFrame,target*CFrame.new(8,0,0))
    same(camera:GetPivot(),target*CFrame.new(2,3,4))
    same(nested.WorldPivot,target*CFrame.new(6,1,0))
    camera:PivotTo(CFrame.new(1,2,3));same(camera:GetPivot(),CFrame.new(1,2,3))
"#);
}

#[test]
fn pivot_to_signals_observe_a_complete_batch_and_suppress_derived_fields() {
    run(r#"
    local a=part(nil,CFrame.new(-2,0,0));local b=part(nil,CFrame.new(2,0,0))
    local cfCount,posCount,orientationCount,allReady=0,0,0,true
    a:GetPropertyChangedSignal("CFrame"):Connect(function()
        cfCount+=1;allReady=allReady and b.Position.X==12 and a.Position.X==8
    end)
    a:GetPropertyChangedSignal("Position"):Connect(function() posCount+=1 end)
    a:GetPropertyChangedSignal("Orientation"):Connect(function() orientationCount+=1 end)
    m:PivotTo(CFrame.new(10,0,0));task.wait()
    assert(cfCount==1 and posCount==0 and orientationCount==0 and allReady)
"#);
}

#[test]
fn repeated_pivots_keep_cached_relative_poses_without_drift() {
    let mut r = runtime();
    execute(
        &mut r,
        &format!(
            "{SETUP}\n{}",
            r#"
    local a=part(nil,CFrame.new(3.123456789,4.987654321,5.13579)*CFrame.Angles(.3,.2,.1))
    local b=part(nil,CFrame.new(-7.2468,2.7654321,9.12345)*CFrame.Angles(.5,-.1,.8))
    m.WorldPivot=CFrame.new()
    shared.case={m=m,a=a,b=b,originalA=a.CFrame,originalB=b.CFrame,same=same}
"#
        ),
    );
    // Drain normal protocol output between ticks, preserving one VM/cache.
    for tick in 0..8 {
        execute(
            &mut r,
            &format!(
                r#"
            local c=shared.case
            for i={start},{end} do
                c.m:PivotTo(CFrame.new(i*.13,math.sin(i),math.cos(i))*CFrame.Angles(i*.011,-i*.017,i*.007))
                c.m:PivotTo(CFrame.new())
            end
            c.same(c.a.CFrame,c.originalA,1e-12);c.same(c.b.CFrame,c.originalB,1e-12)
        "#,
                start = tick * 50 + 1,
                end = (tick + 1) * 50
            ),
        );
    }
}

#[test]
fn manual_edits_new_parts_and_reparenting_invalidate_cached_offsets() {
    run(r#"
    local a=part(nil,CFrame.new(1,0,0));m.WorldPivot=CFrame.new()
    m:PivotTo(CFrame.new(10,0,0));a.CFrame=CFrame.new(20,0,0)
    local b=part(nil,CFrame.new(30,0,0))
    m:PivotTo(CFrame.new(15,0,0));near(a.Position.X,25);near(b.Position.X,35)
    b.Parent=workspace;m:PivotTo(CFrame.new(20,0,0));near(a.Position.X,30);near(b.Position.X,35)
    a.PivotOffset=CFrame.new(2,0,0);m.PrimaryPart=a
    m:PivotTo(CFrame.new(42,0,0));near(a.Position.X,40);near(m:GetPivot().X,42)
"#);
}

#[test]
fn cloned_models_preserve_automatic_and_explicit_identity_pivot_modes() {
    run(r#"
    local p=part(nil,CFrame.new(10,0,0));p.Name="P"
    local auto=m:Clone();auto.P.CFrame=CFrame.new(20,0,0);near(auto:GetPivot().X,20)
    m.WorldPivot=CFrame.new();local explicit=m:Clone()
    explicit.P.CFrame=CFrame.new(30,0,0);same(explicit:GetPivot(),CFrame.new())
    explicit:PivotTo(CFrame.new(5,0,0));near(explicit.P.Position.X,35)
"#);
}

#[test]
fn rotated_bounds_match_an_independent_eight_corner_reference() {
    run(r#"
    local parts={part(nil,CFrame.new(-3,5,2)*CFrame.Angles(.2,.4,.6),Vector3.new(4,2,6)),
        part(nil,CFrame.new(8,-2,7)*CFrame.Angles(.7,.1,.3),Vector3.new(2,5,3))}
    local axes=CFrame.new(12,7,-4)*CFrame.Angles(.8,-.2,.3);m.WorldPivot=axes
    local low=Vector3.new(math.huge,math.huge,math.huge);local high=-low
    for _,p in parts do for x=-1,1,2 do for y=-1,1,2 do for z=-1,1,2 do
        local corner=axes:PointToObjectSpace(p.CFrame:PointToWorldSpace(p.Size*Vector3.new(x,y,z)/2))
        low=Vector3.new(math.min(low.X,corner.X),math.min(low.Y,corner.Y),math.min(low.Z,corner.Z))
        high=Vector3.new(math.max(high.X,corner.X),math.max(high.Y,corner.Y),math.max(high.Z,corner.Z))
    end end end end
    local cf,size=m:GetBoundingBox();same(cf,axes*CFrame.new((low+high)/2))
    near(size.X,high.X-low.X);near(size.Y,high.Y-low.Y);near(size.Z,high.Z-low.Z)
"#);
}

#[test]
fn empty_models_have_a_usable_pivot_and_zero_bounds() {
    run(r#"
    same(m:GetPivot(),CFrame.new());assert(m:GetExtentsSize()==Vector3.zero)
    local target=CFrame.new(20,30,40)*CFrame.Angles(.1,.2,.3);m:PivotTo(target)
    same(m:GetPivot(),target);local cf,size=m:GetBoundingBox();same(cf,target);assert(size==Vector3.zero)
    local p=part(nil,CFrame.new(100,0,0));same(m:GetPivot(),target)
"#);
}

#[test]
fn legacy_translation_moves_explicit_pivots_without_freezing_new_models() {
    run(r#"
    local camera=Instance.new("Camera",m);camera.CFrame=CFrame.new(1,2,3)
    local p=part(nil,CFrame.new(10,0,0));m:TranslateBy(Vector3.new(3,4,5))
    near(p.Position.X,13);near(m:GetPivot().X,13)
    p.CFrame=CFrame.new(20,0,0);near(m:GetPivot().X,20)
    m.WorldPivot=CFrame.new(5,0,0);m:TranslateBy(Vector3.new(2,0,0))
    near(p.Position.X,22);near(m.WorldPivot.X,7)
    same(camera.CFrame,CFrame.new(1,2,3))
    m.PrimaryPart=p;m:SetPrimaryPartCFrame(CFrame.new(50,0,0))
    near(p.Position.X,50);same(camera.CFrame,CFrame.new(1,2,3))
"#);
}

#[test]
fn invalid_transforms_receivers_and_external_primaries_do_not_mutate() {
    run(r#"
    local p=part(nil,CFrame.new(10,0,0));local before=p.CFrame
    for _,call in {
        function() m:PivotTo(Vector3.zero) end,
        function() m:PivotTo(CFrame.new(0/0,0,0)) end,
        function() m:PivotTo(CFrame.new(0,0,0,2,0,0,0,1,0,0,0,1)) end,
        function() m.WorldPivot=Vector3.zero end,
        function() p.PivotOffset=CFrame.new(math.huge,0,0) end,
        function() m:GetPivot(1) end,
        function() m:GetPrimaryPartCFrame() end,
    } do assert(not pcall(call));same(p.CFrame,before) end
    local outside=part(workspace,CFrame.new(90,0,0));m.PrimaryPart=outside
    assert(not pcall(function() m:PivotTo(CFrame.new()) end));same(p.CFrame,before)
    m:Destroy();assert(not pcall(function() m:GetPivot() end))
"#);
}

#[test]
fn nested_automatic_pivots_fold_bounds_without_quadratic_world_scans() {
    run(r#"
    local child=m
    for i=1,250 do child=Instance.new("Model",child) end
    local p=part(child,CFrame.new(30,40,50));near(m:GetPivot().X,30)
    m:PivotTo(CFrame.new(2,3,4));same(p.CFrame,CFrame.new(2,3,4));same(child:GetPivot(),CFrame.new(2,3,4))
"#);
}

#[test]
fn invalid_received_descendant_rejects_a_move_before_any_field_changes() {
    let mut r=Runtime::new(json!({"version":1,"sequence":1,"services":{"Workspace":"workspace"},"nodes":[
        {"id":"workspace","class_name":"Workspace","name":"Workspace","parent":null,"properties":{}},
        {"id":"model","class_name":"Model","name":"Mover","parent":"workspace","properties":{"PrimaryPart":null,"WorldPivot":{"$cframe":{"position":[0,0,0],"rotation":[1,0,0,0,1,0,0,0,1]}}}},
        {"id":"a","class_name":"Part","name":"A","parent":"model","properties":{"CFrame":{"$cframe":{"position":[10,0,0],"rotation":[1,0,0,0,1,0,0,0,1]}}}},
        {"id":"z","class_name":"Part","name":"Z","parent":"model","properties":{}}
    ]}),32).unwrap();
    execute(
        &mut r,
        r#"
        local m=workspace.Mover;local before=m.A.CFrame
        local fired=0;m.A.Changed:Connect(function() fired+=1 end)
        local ok,e=pcall(function() m:PivotTo(CFrame.new(20,0,0)) end)
        assert(not ok and string.find(e,"CFrame"));assert(m.A.CFrame==before)
        task.wait();assert(fired==0)
    "#,
    );
}

#[test]
fn received_creates_moves_and_removals_change_bounds_and_invalidate_cache() {
    let mut r = runtime();
    execute(
        &mut r,
        r#"
        m=Instance.new("Model",workspace);m.Name="Mover"
        p=Instance.new("Part",m);p.Size=Vector3.new(2,2,2);p.CFrame=CFrame.new()
        m.WorldPivot=CFrame.new();m:PivotTo(CFrame.new(10,0,0))
        p.Name="P"
    "#,
    );
    // Received children may be adopted into a received model; use a separate
    // complete receipt to exercise the public runtime update path.
    let delta = json!({"version":1,"delta":true,"sequence":3,"actions":[
        {"sequence":2,"kind":"create","node":{"id":"received-model","class_name":"Model","name":"Received","parent":"workspace","properties":{"PrimaryPart":null,"WorldPivot":{"$cframe":{"position":[0,0,0],"rotation":[1,0,0,0,1,0,0,0,1]}}}}},
        {"sequence":3,"kind":"create","node":{"id":"received-part","class_name":"Part","name":"P","parent":"received-model","properties":{"CFrame":{"$cframe":{"position":[6,0,0],"rotation":[1,0,0,0,1,0,0,0,1]}},"Size":{"$vector3":[4,2,2]}}}}
    ]});
    let result = r.call(json!({"op":"update","delta":delta})).unwrap();
    assert_eq!(result["ok"], true, "{result}");
    execute(
        &mut r,
        r#"
        local cf,size=workspace.Received:GetBoundingBox();assert(cf.X==6 and size.X==4)
        workspace.Received:PivotTo(CFrame.new(10,0,0));assert(workspace.Received.P.CFrame.X==16)
    "#,
    );
    let update = json!({"version":1,"delta":true,"sequence":4,"actions":[{"sequence":4,"kind":"set","id":"received-part","fields":{"CFrame":{"$cframe":{"position":[30,0,0],"rotation":[1,0,0,0,1,0,0,0,1]}}}}]});
    let result = r.call(json!({"op":"update","delta":update})).unwrap();
    assert_eq!(result["ok"], true, "{result}");
    execute(
        &mut r,
        "workspace.Received:PivotTo(CFrame.new(20,0,0));assert(workspace.Received.P.CFrame.X==40)",
    );
    let result=r.call(json!({"op":"update","delta":{"version":1,"delta":true,"sequence":5,"actions":[{"sequence":5,"kind":"remove","id":"received-part"}]}})).unwrap();
    assert_eq!(result["ok"], true, "{result}");
    execute(&mut r,"assert(workspace.Received:GetExtentsSize()==Vector3.zero);assert(workspace.Received:GetPivot().X==20)");
}

#[test]
fn an_unreceived_pivot_stays_unknown_when_a_received_model_is_cloned() {
    let mut r=Runtime::new(json!({"version":1,"sequence":1,"services":{"Workspace":"workspace"},"nodes":[
        {"id":"workspace","class_name":"Workspace","name":"Workspace","parent":null,"properties":{}},
        {"id":"model","class_name":"Model","name":"Received","parent":"workspace","properties":{"PrimaryPart":null,"Archivable":true}}
    ]}),32).unwrap();
    execute(
        &mut r,
        r#"
        local m=workspace.Received
        assert(not pcall(function() m:GetPivot() end))
        local clone=m:Clone()
        assert(not pcall(function() clone:GetPivot() end))
        clone.WorldPivot=CFrame.new(20,0,0);assert(clone:GetPivot().X==20)
    "#,
    );
}

#[test]
fn shipped_pivot_smoke_runs_through_the_official_vm() {
    let fixture: Value =
        serde_json::from_str(include_str!("../../tests/fixtures/world_snapshot.json")).unwrap();
    let result = execute(
        &mut Runtime::new(fixture, 32).unwrap(),
        include_str!("../../examples/model-pivots-smoke.luau"),
    );
    assert_eq!(
        result["values"][0]["passed"].as_f64(),
        Some(12.),
        "{result}"
    );
}
