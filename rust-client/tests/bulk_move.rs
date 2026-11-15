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
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        let reply = r.status(id).unwrap();
        assert_eq!(reply["ok"], true, "{reply}");
        if reply["completed"] == true {
            return reply;
        }
        assert!(Instant::now() < end, "{reply}");
        r.poll().unwrap();
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn run(source: &str) {
    execute(&mut runtime(), source);
}

#[test]
fn packaged_bulk_move_smoke_runs_with_real_luau() {
    let result = execute(
        &mut runtime(),
        include_str!("../../examples/bulk-move-smoke.luau"),
    );
    assert_eq!(
        result["values"][0]["passed"].as_f64(),
        Some(20.),
        "{result}"
    );
}

#[test]
fn bulk_event_modes_callbacks_see_complete_batch_and_noop_is_silent() {
    run(&format!(
        "{RIG}\n{}",
        r#"
        local point=Instance.new("Attachment",b)
        local counts={CFrame=0,Position=0,Orientation=0,Rotation=0,WorldPosition=0}
        local expected;local coherent=true
        for _,key in {"CFrame","Position","Orientation","Rotation"} do
            a:GetPropertyChangedSignal(key):Connect(function()
                counts[key]+=1
                coherent=coherent and pcall(frame,c.CFrame,expected*CFrame.new(8,0,0))
            end)
        end
        point:GetPropertyChangedSignal("WorldPosition"):Connect(function()counts.WorldPosition+=1 end)
        expected=CFrame.new(10,20,30)*CFrame.Angles(.2,.4,.6)
        workspace:BulkMoveTo({a},{expected},Enum.BulkMoveMode.FireCFrameChanged)
        task.wait()
        assert(counts.CFrame==1 and counts.WorldPosition==1)
        assert(counts.Position==0 and counts.Orientation==0 and counts.Rotation==0)
        workspace:BulkMoveTo({a},{expected})
        task.wait();assert(counts.CFrame==1 and counts.WorldPosition==1)
        expected=CFrame.new(30,40,50)*CFrame.Angles(.6,.1,.9)
        workspace:BulkMoveTo({a},{expected})
        task.wait()
        assert(counts.CFrame==2 and counts.WorldPosition==2)
        assert(counts.Position==1 and counts.Orientation==1 and counts.Rotation==1)
        assert(coherent)
    "#
    ));
}

#[test]
fn graph_derived_signals_follow_enabled_root_priority_and_parent() {
    run(&format!(
        "{RIG}\n{}",
        r#"
        local active,roots=0,0
        ab:GetPropertyChangedSignal("Active"):Connect(function()active+=1 end)
        a:GetPropertyChangedSignal("AssemblyRootPart"):Connect(function()roots+=1 end)
        b.RootPriority=12;task.wait()
        assert(a.AssemblyRootPart==b and a:GetRootPart()==b and roots==1)
        ab.Enabled=false;task.wait()
        assert(active==1 and roots==2 and a.AssemblyRootPart==a)
        ab.Enabled=true;task.wait();assert(active==2 and roots==3)
        a.Parent=nil;task.wait();assert(active==3 and roots==4 and a.AssemblyRootPart==nil)
        a.Parent=workspace;task.wait();assert(active==4 and roots==5)
        assert(not pcall(function()a.AssemblyRootPart=b end))
        assert(not pcall(function()ab.Part1=Instance.new("Folder")end))
    "#
    ));
}

#[test]
fn graph_alignment_and_reference_attachment_modes_register_correctly() {
    run(r#"
        local p=Instance.new("Part",workspace);local q=Instance.new("Part",workspace)
        local a=Instance.new("Attachment",p);local b=Instance.new("Attachment",q)
        for _,class in {"VectorForce","Torque","LinearVelocity","AngularVelocity"} do
            local f=Instance.new(class,workspace);f.Attachment0=a
            f.RelativeTo=Enum.ActuatorRelativeTo.World
            assert(not pcall(function()f.RelativeTo=Enum.Material.Plastic end))
            assert(not pcall(function()f.RelativeTo=2 end))
            assert(f.Active and table.find(p:GetJoints(),f) and not table.find(q:GetJoints(),f))
            f.RelativeTo=Enum.ActuatorRelativeTo.Attachment1;assert(not f.Active)
            f.Attachment1=b;assert(f.Active and table.find(q:GetJoints(),f))
            assert(#p:GetConnectedParts(true)==0)
            f.Enabled=false;assert(not f.Active)
        end
        for _,class in {"AlignPosition","AlignOrientation"} do
            local c=Instance.new(class,workspace);c.Attachment0=a
            assert(not c.Active)
            c.Attachment1=b;assert(c.Active)
            c.Attachment1=nil;c.Mode=class=="AlignPosition" and Enum.PositionAlignmentMode.OneAttachment or Enum.OrientationAlignmentMode.OneAttachment
            assert(c.Active)
        end
    "#);
}

#[test]
fn graph_jointsservice_only_connects_workspace_parts() {
    run(r#"
        local service=game:GetService("JointsService")
        local p=Instance.new("Part",workspace);local q=Instance.new("Part",workspace)
        local w=Instance.new("Weld",service);w.Part0=p;w.Part1=q
        assert(w.Active and p:GetConnectedParts()[1]==q)
        local m=Instance.new("WorldModel",workspace);p.Parent=m;q.Parent=m
        assert(not w.Active and #p:GetConnectedParts()==0)
        w.Parent=m;assert(w.Active)
    "#);
}

fn pose(x: f64) -> Value {
    json!({"$cframe":{"position":[x,0,0],"rotation":[1,0,0,0,1,0,0,0,1]}})
}
fn received_rig() -> Runtime {
    Runtime::new(json!({"version":1,"sequence":1,"services":{"Workspace":"workspace"},"nodes":[
        {"id":"workspace","class_name":"Workspace","name":"Workspace","properties":{}},
        {"id":"a","class_name":"Part","name":"A","parent":"workspace","properties":{"CFrame":pose(0.),"Anchored":false}},
        {"id":"b","class_name":"Part","name":"B","parent":"workspace","properties":{"Anchored":false}},
        {"id":"c","class_name":"Part","name":"C","parent":"workspace","properties":{"CFrame":pose(20.),"Anchored":false}},
        {"id":"w","class_name":"Weld","name":"W","parent":"a","properties":{"Part0":{"$ref":"a"},"Part1":{"$ref":"b"},"Enabled":true}}
    ]}),32).unwrap()
}

#[test]
fn bulk_unknown_partner_rejects_entire_batch_and_isolated_target_can_repair_pose() {
    let mut r = received_rig();
    execute(
        &mut r,
        r#"
        assert(not pcall(function()workspace:BulkMoveTo({workspace.C,workspace.A},{CFrame.new(100,0,0),CFrame.new(50,0,0)})end))
        assert(workspace.C.CFrame.X==20 and workspace.A.CFrame.X==0)
        workspace.A.W.Enabled=false
        workspace:BulkMoveTo({workspace.B},{CFrame.new(7,0,0)})
        assert(workspace.B.CFrame.X==7 and workspace.B.Position.X==7)
    "#,
    );
}

#[test]
fn received_cframe_replaces_local_batch_aliases_and_attachment_world_pose() {
    let mut r = received_rig();
    execute(
        &mut r,
        r#"
        workspace.A.W.Enabled=false
        workspace:BulkMoveTo({workspace.A},{CFrame.new(50,0,0)*CFrame.Angles(.2,.4,.6)})
        local at=Instance.new("Attachment",workspace.A);at.Name="Point";at.Position=Vector3.new(1,0,0)
        getgenv().pointChanges=0
        getgenv().positionChanges=0
        at:GetPropertyChangedSignal("WorldPosition"):Connect(function()pointChanges+=1 end)
        workspace.A:GetPropertyChangedSignal("Position"):Connect(function()positionChanges+=1 end)
    "#,
    );
    let reply = r
        .call(
            json!({"op":"update","delta":{"version":1,"delta":true,"sequence":2,"actions":[
                {"kind":"set","sequence":2,"id":"a","fields":{"CFrame":pose(90.)}}
            ]}}),
        )
        .unwrap();
    assert_eq!(reply["ok"], true, "{reply}");
    execute(
        &mut r,
        r#"
        assert(workspace.A.CFrame.X==90 and workspace.A.Position.X==90)
        vec(workspace.A.Orientation,Vector3.zero);vec(workspace.A.Rotation,Vector3.zero)
        assert(workspace.A.Point.WorldPosition.X==91)
        task.wait();assert(pointChanges==1 and positionChanges==1)
    "#,
    );
}

#[test]
fn received_missing_constraint_mode_and_rotation_are_not_defaulted() {
    let mut r = received_rig();
    let reply=r.call(json!({"op":"update","delta":{"version":1,"delta":true,"sequence":4,"actions":[
        {"kind":"create","sequence":2,"node":{"id":"at","class_name":"Attachment","name":"Point","parent":"c","properties":{}}},
        {"kind":"create","sequence":3,"node":{"id":"vf","class_name":"VectorForce","name":"Force","parent":"c","properties":{"Attachment0":{"$ref":"at"},"Attachment1":null,"Enabled":true}}},
        {"kind":"set","sequence":4,"id":"c","fields":{"CFrame":{"$cframe":{"position":[20,0,0],"rotation_id":17}}}}
    ]}})).unwrap();
    assert_eq!(reply["ok"], true, "{reply}");
    execute(
        &mut r,
        r#"
        assert(not pcall(function()return workspace.C.Force.Active end))
        assert(not pcall(function()return workspace.C:GetJoints()end))
        assert(workspace.C.Position.X==20)
        assert(not pcall(function()return workspace.C.Orientation end))
        workspace.C.Force.RelativeTo=Enum.ActuatorRelativeTo.World
        assert(workspace.C.Force.Active)
        workspace:BulkMoveTo({workspace.C},{CFrame.new(60,0,0)})
        assert(workspace.C.Position.X==60)
    "#,
    );
}
const HELPERS: &str = r###"
local function near(a,b) assert(math.abs(a-b)<2e-5, tostring(a).." != "..tostring(b)) end
local function vec(a,b) near(a.X,b.X); near(a.Y,b.Y); near(a.Z,b.Z) end
local function frame(a,b)
    local aa,bb={a:GetComponents()},{b:GetComponents()}
    for i=1,12 do near(aa[i],bb[i]) end
end
"###;
const RIG: &str = r###"
local function part(name,pos,parent)
 local p=Instance.new("Part",parent or workspace);p.Name=name;p.CFrame=CFrame.new(pos);return p
end
local function join(class,a,b,parent)
 local j=Instance.new(class,parent or a);j.Part0=a;j.Part1=b;return j
end
local a=part("A",Vector3.zero)
local b=part("B",Vector3.new(4,0,0))
local c=part("C",Vector3.new(8,0,0))
local ab=join("Weld",a,b);ab.C0=CFrame.new(4,0,0)
local bc=join("Motor6D",b,c);bc.C0=CFrame.new(4,0,0)
local function contains(list,value)for _,v in list do if v==value then return true end end;return false end
"###;
const SETUP: &str = r###"
local function part(position,size,parent,kind)
 local p=Instance.new(kind or "Part",parent or workspace)
 p.Anchored=true;p.Size=size or Vector3.new(2,2,2);p.CFrame=CFrame.new(position)
 return p
end
local function contains(list,p) return table.find(list,p)~=nil end
local function box(cf,size,params) return workspace:GetPartBoundsInBox(cf or CFrame.identity,size or Vector3.new(10,10,10),params) end
local function sphere(center,radius,params) return workspace:GetPartBoundsInRadius(center or Vector3.zero,radius or 5,params) end
"###;

#[test]
fn bulk_enum_metadata_and_empty_batch() {
    run(&[SETUP, r###"
local mode=Enum.BulkMoveMode
assert(typeof(mode)=="Enum" and typeof(mode.FireAllEvents)=="EnumItem")
assert(mode.FireAllEvents.Value==0 and mode.FireCFrameChanged.Value==1)
assert(mode.FireAllEvents.EnumType==mode and tostring(mode.FireCFrameChanged)=="Enum.BulkMoveMode.FireCFrameChanged")
assert(#mode:GetEnumItems()==2)
assert(mode:FromName("FireAllEvents")==mode.FireAllEvents)
assert(mode:FromValue(1)==mode.FireCFrameChanged)
assert(not pcall(function()mode.FireAllEvents.Value=1 end))
assert(select('#',workspace:BulkMoveTo({},{}))==0)
assert(select('#',workspace:BulkMoveTo({},{},mode.FireCFrameChanged))==0)
"###].concat());
}

#[test]
fn bulk_moves_arrays_and_keeps_all_transform_fields_coherent() {
    run(&[SETUP, r###"
local a=part(Vector3.zero);local b=part(Vector3.new(3,0,0))
local targets={CFrame.new(10,20,30)*CFrame.Angles(.2,.3,.4),CFrame.new(-4,5,6)*CFrame.Angles(-.4,.3,.2)}
local parts={a,b};workspace:BulkMoveTo(parts,targets)
for i,p in parts do
 frame(p.CFrame,targets[i]);vec(p.Position,targets[i].Position)
 local x,y,z=targets[i]:ToOrientation();vec(p.Orientation,Vector3.new(math.deg(x),math.deg(y),math.deg(z)))
 x,y,z=targets[i]:ToEulerAnglesXYZ();vec(p.Rotation,Vector3.new(math.deg(x),math.deg(y),math.deg(z)))
end
assert(parts[1]==a and targets[1].Position==Vector3.new(10,20,30))
"###].concat());
}

#[test]
fn bulk_duplicates_and_conflicting_connected_targets_use_last_request() {
    run(&[
        SETUP,
        r###"
local p=part(Vector3.zero)
workspace:BulkMoveTo({p,p},{CFrame.new(1,0,0),CFrame.new(2,0,0)})
vec(p.Position,Vector3.new(2,0,0))
"###,
    ]
    .concat());
    run(&[RIG, r###"
workspace:BulkMoveTo({a,b},{CFrame.new(10,0,0),CFrame.new(30,0,0)})
vec(a.Position,Vector3.new(26,0,0));vec(b.Position,Vector3.new(30,0,0));vec(c.Position,Vector3.new(34,0,0))
"###].concat());
}

#[test]
fn bulk_welded_parts_and_attachment_follow_one_rigid_delta() {
    run(&[RIG, r###"
local at=Instance.new("Attachment",c);at.CFrame=CFrame.new(1,2,3)
local target=CFrame.new(10,20,30)*CFrame.Angles(.3,.8,-.2)
workspace:BulkMoveTo({a},{target},Enum.BulkMoveMode.FireCFrameChanged)
frame(a.CFrame,target);frame(b.CFrame,target*CFrame.new(4,0,0));frame(c.CFrame,target*CFrame.new(8,0,0))
frame(at.WorldCFrame,c.CFrame*at.CFrame)
assert(ab.Part0==a and ab.Part1==b and bc.Part0==b and bc.Part1==c)
"###].concat());
}

#[test]
fn bulk_multiple_coherent_targets_do_not_apply_assembly_delta_twice() {
    run(&[
        RIG,
        r###"
local delta=CFrame.new(10,20,30)*CFrame.Angles(.4,.5,.6)
local targets={delta*a.CFrame,delta*b.CFrame,delta*c.CFrame}
workspace:BulkMoveTo({a,b,c},targets)
frame(a.CFrame,targets[1]);frame(b.CFrame,targets[2]);frame(c.CFrame,targets[3])
"###,
    ]
    .concat());
}

#[test]
fn bulk_disabled_and_flexible_joints_do_not_move_other_parts() {
    run(&[
        RIG,
        r###"
ab.Enabled=false;bc.Enabled=false
local aa=Instance.new("Attachment",a);local cc=Instance.new("Attachment",c)
local spring=Instance.new("SpringConstraint",workspace);spring.Attachment0=aa;spring.Attachment1=cc
local beforeB,beforeC=b.CFrame,c.CFrame
workspace:BulkMoveTo({a},{CFrame.new(10,0,0)})
frame(b.CFrame,beforeB);frame(c.CFrame,beforeC)
"###,
    ]
    .concat());
}

#[test]
fn bulk_detached_parts_and_worldmodel_receiver() {
    run(&[
        SETUP,
        r###"
local model=Instance.new("WorldModel");local a=part(Vector3.zero,nil,model)
local b=part(Vector3.zero);b.Parent=nil
model:BulkMoveTo({a,b},{CFrame.new(1,2,3),CFrame.new(4,5,6)})
vec(a.Position,Vector3.new(1,2,3));vec(b.Position,Vector3.new(4,5,6))
assert(a.Parent==model and b.Parent==nil)
model:Destroy();assert(not pcall(function()model:BulkMoveTo({},{})end))
"###,
    ]
    .concat());
}

#[test]
fn bulk_queries_see_moved_geometry_immediately() {
    run(&[
        SETUP,
        r###"
local p=part(Vector3.new(20,0,0));p.Material=Enum.Material.Metal
assert(#box()==0 and workspace:Raycast(Vector3.zero,Vector3.new(0,0,-20))==nil)
workspace:BulkMoveTo({p},{CFrame.new(0,0,-5)})
assert(box(CFrame.new(0,0,-5),Vector3.one)[1]==p)
assert(sphere(Vector3.new(0,0,-5),1)[1]==p)
assert(workspace:Raycast(Vector3.zero,Vector3.new(0,0,-20)).Instance==p)
"###,
    ]
    .concat());
}

#[test]
fn bulk_entire_batch_validation_precedes_any_mutation() {
    run(&[
        SETUP,
        r###"
local a=part(Vector3.zero);local b=part(Vector3.new(3,0,0));local oldA,oldB=a.CFrame,b.CFrame
local target=CFrame.new(100,100,100);local folder=Instance.new("Folder")
local dead=Instance.new("Part");dead:Destroy()
for _,bad in {
 function()workspace:BulkMoveTo({a,folder},{target,target})end,
 function()workspace:BulkMoveTo({a,dead},{target,target})end,
 function()workspace:BulkMoveTo({a,b},{target,Vector3.zero})end,
 function()workspace:BulkMoveTo({a,b},{target,CFrame.new(math.huge,0,0)})end,
 function()workspace:BulkMoveTo({a,b},{target,CFrame.new(0,0,0,0/0,0,0,0,1,0,0,0,1)})end,
 function()workspace:BulkMoveTo({a,b},{target})end,
 function()workspace:BulkMoveTo({a},{target,target})end,
 function()workspace:BulkMoveTo({[1]=a,[3]=b},{target,target,target})end,
 function()workspace:BulkMoveTo({a},{[1]=target,extra=target})end,
 function()workspace:BulkMoveTo({a},{target},Enum.NormalId.Top)end,
 function()workspace:BulkMoveTo({a},{target},0)end,
 function()workspace:BulkMoveTo({a},{target},"FireAllEvents")end,
 function()workspace:BulkMoveTo(a,{target})end,
 function()workspace:BulkMoveTo({a},target)end,
 function()workspace.BulkMoveTo(a,{a},{target})end,
 function()workspace:BulkMoveTo({a})end,
 function()workspace:BulkMoveTo({a},{target},nil,1)end,
} do assert(not pcall(bad));frame(a.CFrame,oldA);frame(b.CFrame,oldB)end
workspace:BulkMoveTo({a,b},{target,target});frame(a.CFrame,target);frame(b.CFrame,target)
"###,
    ]
    .concat());
}

#[test]
fn bulk_array_metatables_are_not_executed_and_capacity_is_bounded() {
    run(&[SETUP, r###"
local p=part(Vector3.zero)
local meta={__index=function()error("must not run")end,__len=function()error("must not run")end}
workspace:BulkMoveTo(setmetatable({p},meta),setmetatable({CFrame.new(3,4,5)},meta))
vec(p.Position,Vector3.new(3,4,5))
workspace:BulkMoveTo(table.create(4096,p),table.create(4096,p.CFrame))
assert(not pcall(function()workspace:BulkMoveTo(table.create(4097,p),table.create(4097,p.CFrame))end))
vec(p.Position,Vector3.new(3,4,5))
"###].concat());
}

#[test]
fn bulk_independent_assemblies_keep_their_respective_rigid_offsets() {
    run(&[RIG, r###"
local d=part("D",Vector3.new(30,0,0));local e=part("E",Vector3.new(33,0,0))
local de=join("Weld",d,e);de.C0=CFrame.new(3,0,0)
local first=CFrame.new(10,20,30)*CFrame.Angles(.2,.5,.7)
local second=CFrame.new(-10,40,5)*CFrame.Angles(-.3,.1,.8)
workspace:BulkMoveTo({a,d},{first,second})
frame(a.CFrame,first);frame(b.CFrame,first*CFrame.new(4,0,0));frame(c.CFrame,first*CFrame.new(8,0,0))
frame(d.CFrame,second);frame(e.CFrame,second*CFrame.new(3,0,0))
"###].concat());
}

#[test]
fn graph_direct_and_recursive_queries_are_symmetric_and_exclude_self() {
    run(&[RIG, r###"
assert(ab.Active and bc.Active)
local direct=a:GetConnectedParts();assert(#direct==1 and direct[1]==b)
local all=a:GetConnectedParts(true);assert(#all==2 and contains(all,b) and contains(all,c))
local reverse=c:GetConnectedParts(true);assert(#reverse==2 and contains(reverse,a) and contains(reverse,b))
assert(#b:GetJoints()==2 and contains(b:GetJoints(),ab) and contains(b:GetJoints(),bc))
local copy=a:GetConnectedParts(true);ab.Enabled=false
assert(#a:GetConnectedParts(true)==0 and #copy==2 and not ab.Active)
assert(#b:GetConnectedParts(true)==1)
assert(not pcall(function()a:GetConnectedParts(1)end))
local query=a.GetJoints;assert(not pcall(query,Instance.new("Folder")))
assert(not pcall(function()ab.Active=true end))"###].concat());
}

#[test]
fn graph_flexible_constraints_are_joints_but_not_rigid_edges() {
    run(&[
        RIG,
        r###"
ab.Enabled=false;bc.Enabled=false
local aa=Instance.new("Attachment",a);local cc=Instance.new("Attachment",c)
local spring=Instance.new("SpringConstraint",workspace);spring.Attachment0=aa;spring.Attachment1=cc
assert(spring.Active and #a:GetJoints()==1 and a:GetJoints()[1]==spring)
assert(#a:GetConnectedParts(true)==0)
local old=c.CFrame;a.CFrame=CFrame.new(5,6,7);frame(c.CFrame,old)
spring.Enabled=false;assert(not spring.Active and #a:GetJoints()==0)"###,
    ]
    .concat());
}

#[test]
fn graph_single_attachment_force_registration_does_not_make_rigid_connection() {
    run(r###"local p=Instance.new("Part",workspace)
local a=Instance.new("Attachment",p);local f=Instance.new("VectorForce",a);f.Attachment0=a
assert(f.Active and #p:GetJoints()==1 and p:GetJoints()[1]==f)
assert(#p:GetConnectedParts(true)==0)
f.Attachment0=nil;assert(not f.Active and #p:GetJoints()==0)"###);
}

#[test]
fn graph_container_scope_parenting_and_destroy_rebuild_connections() {
    run(&[
        RIG,
        r###"
ab.Parent=nil;assert(not ab.Active and #a:GetJoints()==0)
ab.Parent=a;assert(ab.Active)
local other=Instance.new("WorldModel",workspace)
c.Parent=other;assert(not bc.Active and #b:GetConnectedParts(true)==1)
b.Parent=other;bc.Parent=b;assert(bc.Active and not ab.Active)
assert(#b:GetConnectedParts(true)==1 and b:GetConnectedParts()[1]==c)
bc:Destroy();assert(#b:GetConnectedParts(true)==0 and #c:GetJoints()==0)
"###,
    ]
    .concat());
}

#[test]
fn graph_duplicate_cycles_and_anchored_islands_are_deterministic() {
    run(&[
        RIG,
        r###"
local ca=join("WeldConstraint",c,a)
assert(ab.Active and bc.Active and not ca.Active)
assert(#a:GetConnectedParts(true)==2)
ab.Enabled=false;assert(bc.Active and ca.Active)
a.Anchored=true;b.Anchored=true
assert(bc.Active and not ca.Active and #a:GetConnectedParts(true)==0)
b.Anchored=false;assert(ca.Active and #a:GetConnectedParts(true)==2)
"###,
    ]
    .concat());
}

#[test]
fn graph_joint_and_attachment_endpoints_enforce_their_classes() {
    run(
        r###"local w=Instance.new("Weld");local f=Instance.new("Folder")
assert(not pcall(function()w.Part0=f end));assert(w.Part0==nil)
local c=Instance.new("HingeConstraint")
assert(not pcall(function()c.Attachment0=Instance.new("Part")end));assert(c.Attachment0==nil)
local a=Instance.new("Attachment");c.Attachment0=a;assert(c.Attachment0==a)
"###,
    );
}

#[test]
fn graph_clone_connection_queries_use_cloned_endpoints() {
    run(r###"local m=Instance.new("Model",workspace)
local a,b=Instance.new("Part",m),Instance.new("Part",m);a.Name="A";b.Name="B"
local j=Instance.new("Weld",a);j.Part0=a;j.Part1=b
local copied=m:Clone();copied.Parent=workspace
assert(copied.A:GetConnectedParts()[1]==copied.B)
copied.A.CFrame=CFrame.new(50,0,0)
vec(copied.B.Position,Vector3.new(50,0,0));vec(a.Position,Vector3.zero);vec(b.Position,Vector3.zero)"###);
}
