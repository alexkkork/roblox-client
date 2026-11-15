#![cfg(feature = "rust-host")]
use headless_luau::runtime_owner::Runtime;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

fn runtime() -> Runtime {
    Runtime::new(json!({"version":1,"sequence":1,"services":{"Workspace":"workspace"},"nodes":[
        {"id":"workspace","class_name":"Workspace","name":"Workspace","parent":null,"properties":{}}
    ]}),32).unwrap()
}
fn execute(runtime: &mut Runtime, source: &str) -> Value {
    let started = runtime
        .call(json!({"op":"execute","source":source,"timeout_ms":2000}))
        .unwrap();
    let id = started["job_id"]
        .as_u64()
        .unwrap_or_else(|| panic!("{started}"));
    let until = Instant::now() + Duration::from_secs(15);
    loop {
        let status = runtime.status(id).unwrap();
        assert_eq!(status["ok"], true, "{status}");
        if status["completed"] == true {
            return status;
        }
        assert!(Instant::now() < until, "{status}");
        runtime.poll().unwrap();
        std::thread::sleep(Duration::from_millis(1));
    }
}
const SETUP: &str = r#"
local m=Instance.new("Model",workspace)
local function part(parent,cf,size)
 local p=Instance.new("Part",parent or m);p.Anchored=true
 p.CFrame=cf or CFrame.new();p.Size=size or Vector3.new(2,3,4);return p
end
local function near(a,b,eps) assert(math.abs(a-b)<(eps or .0001),tostring(a).." ~= "..tostring(b)) end
local function same(a,b,eps)
 if typeof(a)=="CFrame" then local av,bv={a:GetComponents()},{b:GetComponents()};for i=1,12 do near(av[i],bv[i],eps) end
 elseif typeof(a)=="Vector3" then near(a.X,b.X,eps);near(a.Y,b.Y,eps);near(a.Z,b.Z,eps)
 else near(a,b,eps) end
end
"#;
fn run(source: &str) {
    execute(&mut runtime(), &format!("{SETUP}\n{source}"));
}

#[test]
fn absolute_scale_round_trips_exactly_and_preserves_rotated_primary_pivot() {
    run(r#"
 local a=part(nil,CFrame.new(102.13,4.751,5.345)*CFrame.Angles(.2,.3,.4),Vector3.new(1.13,2.27,3.91))
 local b=part(nil,CFrame.new(120.215,7.12,11.54)*CFrame.Angles(-.2,.1,.8))
 a.PivotOffset=CFrame.new(.713,2.13,-1.791)*CFrame.Angles(.3,.2,.1);m.PrimaryPart=a
 local cf,cfb,sz,offset,pivot=a.CFrame,b.CFrame,a.Size,a.PivotOffset,m:GetPivot()
 for i=1,30 do m:ScaleTo(.137);m:ScaleTo(3.797);m:ScaleTo(1) end
 assert(a.CFrame==cf and b.CFrame==cfb and a.Size==sz and a.PivotOffset==offset)
 same(m:GetPivot(),pivot);assert(m:GetScale()==1)
 m:ScaleTo(2);same(m:GetPivot(),pivot);same(a.Size,sz*2)
 local scaled=a.CFrame;m:ScaleTo(2);assert(a.CFrame==scaled)
"#);
}
#[test]
fn automatic_and_explicit_pivots_scale_bounds() {
    run(r#"
 local a=part(nil,CFrame.new(-2,0,0),Vector3.new(2,2,2))
 local b=part(nil,CFrame.new(4,0,0),Vector3.new(4,2,2))
 local original=m:GetPivot();local bounds,size=m:GetBoundingBox()
 m:ScaleTo(2);same(m:GetPivot(),original);same(m:GetExtentsSize(),size*2)
 m:ScaleTo(1);same(a.Position,Vector3.new(-2,0,0));same(b.Position,Vector3.new(4,0,0))
 m.WorldPivot=CFrame.new(1,2,3)*CFrame.Angles(.2,.4,.1);m:ScaleTo(.5)
 same(a.Position,Vector3.new(-.5,1,1.5));same(m:GetPivot(),m.WorldPivot)
"#);
}
#[test]
fn nested_models_and_edited_added_removed_parts_rebase() {
    run(r#"
 m.WorldPivot=CFrame.new();local n=Instance.new("Model",m);n.WorldPivot=CFrame.new(3,0,0)
 local a=part(n,CFrame.new(5,0,0));local b=part(nil,CFrame.new(10,0,0))
 m:ScaleTo(2);assert(n:GetScale()==2);same(n.WorldPivot,CFrame.new(6,0,0))
 a.Size=Vector3.new(8,8,8);local c=part(nil,CFrame.new(20,0,0));b.Parent=workspace
 m:ScaleTo(4);same(a.Size,Vector3.new(16,16,16));same(c.Position,Vector3.new(40,0,0));same(b.Position,Vector3.new(20,0,0))
 assert(n:GetScale()==4);m:ScaleTo(2);same(a.Size,Vector3.new(8,8,8));same(c.Position,Vector3.new(20,0,0))
 n:ScaleTo(1);assert(n:GetScale()==1);same(a.Size,Vector3.new(4,4,4))
"#);
}
#[test]
fn cloned_scaled_models_keep_independent_original_geometry() {
    run(r#"
 local a=part(nil,CFrame.new(.131,2.159,3.211),Vector3.new(1.135,2.381,3.137));a.Name="A"
 m.WorldPivot=CFrame.new(.171,2.33,3.12);local cf,sz=a.CFrame,a.Size
 m:ScaleTo(3.79);local copy=m:Clone();assert(copy:GetScale()==3.79)
 copy:ScaleTo(1);assert(copy.A.CFrame==cf and copy.A.Size==sz)
 copy.A.Size=Vector3.new(2,2,2);copy:ScaleTo(2);m:ScaleTo(1)
 assert(a.CFrame==cf and a.Size==sz and copy.A.Size==Vector3.new(4,4,4))
"#);
}
#[test]
fn joints_bones_effects_and_mesh_dimensions_scale_once() {
    run(r#"
 m.WorldPivot=CFrame.new();local p=part();local a=Instance.new("Attachment",p);a.CFrame=CFrame.new(1,2,3)*CFrame.Angles(.2,.3,.4)
 local bone=Instance.new("Bone",a);bone.Transform=CFrame.new(2,3,4)
 local motor=Instance.new("Motor6D",p);motor.C0=CFrame.new(2,3,4);motor.C1=CFrame.new(-1,-2,-3);motor.Transform=CFrame.new(.5,0,0)
 local file=Instance.new("SpecialMesh",p);file.MeshType=Enum.MeshType.FileMesh;file.Scale=Vector3.new(2,3,4);file.Offset=Vector3.new(1,2,3)
 local box=Instance.new("BlockMesh",p);box.Scale=Vector3.new(3,4,5)
 local emitter=Instance.new("ParticleEmitter",a);emitter.Size=NumberSequence.new({NumberSequenceKeypoint.new(0,2,.5),NumberSequenceKeypoint.new(1,3,1)})
 emitter.Speed=NumberRange.new(2,6);emitter.Acceleration=Vector3.new(1,2,3)
 local beam=Instance.new("Beam",p);beam.Width0=3;beam.CurveSize1=4
 local sound=Instance.new("Sound",p);sound.RollOffMaxDistance=100
 local before=a.CFrame;m:ScaleTo(2)
 same(a.CFrame.Position,Vector3.new(2,4,6));same(a.Position,a.CFrame.Position)
 same(bone.Transform.Position,Vector3.new(4,6,8));same(motor.C0.Position,Vector3.new(4,6,8));same(motor.C1.Position,Vector3.new(-2,-4,-6));same(motor.Transform.Position,Vector3.new(1,0,0))
 same(file.Scale,Vector3.new(4,6,8));same(box.Scale,Vector3.new(3,4,5));assert(emitter.Size.Keypoints[1].Value==4 and emitter.Size.Keypoints[1].Envelope==1)
 assert(emitter.Speed.Min==4 and emitter.Speed.Max==12 and beam.Width0==6 and beam.CurveSize1==8 and sound.RollOffMaxDistance==200)
 m:ScaleTo(1);assert(a.CFrame==before)
"#);
}
#[test]
fn callbacks_observe_complete_geometry_and_scaled_mass() {
    run(r#"
 m.WorldPivot=CFrame.new();local a=part();local b=part(nil,CFrame.new(10,0,0));local mass=a:GetMass()
 local calls=0;local good=true
 a:GetPropertyChangedSignal("Size"):Connect(function() calls+=1;good=good and b.Position.X==20 and m:GetScale()==2 end)
 m:ScaleTo(2);task.wait();assert(calls==1 and good);near(a:GetMass(),mass*8,.01)
 local hit=workspace:Raycast(Vector3.new(20,20,0),Vector3.new(0,-40,0));assert(hit and hit.Instance==b)
"#);
}
#[test]
fn invalid_arguments_overflow_and_unsupported_geometry_are_atomic() {
    run(r#"
 m.WorldPivot=CFrame.new();local a=part();local cf,sz=a.CFrame,a.Size
 for _,v in {0,-1,math.huge,0/0,"2",{},false} do assert(not pcall(function() m:ScaleTo(v) end)) end
 assert(not pcall(function() m:ScaleTo() end));assert(not pcall(function() m:ScaleTo(2,3) end))
 assert(not pcall(function() return m:GetScale(2) end));assert(not pcall(function() return m.Scale end));assert(not pcall(function() m.Scale=2 end))
 assert(not pcall(function() m:ScaleTo(1e38) end));assert(a.Size==sz and a.CFrame==cf and m:GetScale()==1)
 local animator=Instance.new("Animator",Instance.new("AnimationController",m))
 assert(not pcall(function() m:ScaleTo(2) end));assert(a.Size==sz and m:GetScale()==1);animator:Destroy()
 local mesh=Instance.new("SpecialMesh",a);mesh.MeshType=Enum.MeshType.Head
 assert(not pcall(function() m:ScaleTo(2) end));assert(a.Size==sz and m:GetScale()==1);mesh:Destroy()
 m:Destroy();assert(not pcall(function() m:ScaleTo(2) end))
"#);
}
#[test]
fn force_infinity_survives_but_nan_rolls_back() {
    run(r#"
 local p=part();local f=Instance.new("BodyForce",p);f.Force=Vector3.new(math.huge,4,6)
 m:ScaleTo(2);assert(f.Force.X==math.huge and f.Force.Y==32 and f.Force.Z==48)
 f.Force=Vector3.new(0/0,4,6);local size=p.Size
 assert(not pcall(function() m:ScaleTo(3) end));assert(p.Size==size and m:GetScale()==2)
"#);
}
#[test]
fn scaling_after_pivot_to_uses_the_new_pivot() {
    run(r#"
 m.WorldPivot=CFrame.new();local p=part(nil,CFrame.new(2,0,0));m:ScaleTo(2)
 m:PivotTo(CFrame.new(10,20,30)*CFrame.Angles(.2,.4,.3));local before=p.CFrame;local pivot=m:GetPivot()
 m:ScaleTo(4);same(p.Position,pivot.Position+(before.Position-pivot.Position)*2)
 m:ScaleTo(2);assert(p.CFrame==before)
"#);
}
#[test]
fn linear_velocity_scales_but_angular_velocity_and_world_targets_do_not() {
    run(r#"
 local p=part();p.AssemblyLinearVelocity=Vector3.new(4,6,8);p.Velocity=Vector3.new(4,6,8)
 p.AssemblyAngularVelocity=Vector3.new(1,2,3);p.RotVelocity=Vector3.new(1,2,3)
 assert(Enum.PositionAlignmentMode.OneAttachment.Value==0 and Enum.PositionAlignmentMode.TwoAttachment.Value==1)
 assert(Enum.VelocityConstraintMode.Line.Value==0 and Enum.VelocityConstraintMode.Plane.Value==1 and Enum.VelocityConstraintMode.Vector.Value==2)
 local position=Instance.new("AlignPosition",p);position.Mode=Enum.PositionAlignmentMode.OneAttachment;position.Position=Vector3.new(10,20,30)
 local force=Instance.new("LineForce",p);force.InverseSquareLaw=true;force.Magnitude=4.25
 local velocity=Instance.new("LinearVelocity",p);velocity.VelocityConstraintMode=Enum.VelocityConstraintMode.Plane
 velocity.PlaneVelocity=Vector2.new(4,6);velocity.MaxPlanarAxesForce=Vector2.new(4,6)
 m:ScaleTo(2)
 same(p.AssemblyLinearVelocity,Vector3.new(8,12,16));same(p.Velocity,Vector3.new(8,12,16))
 same(p.AssemblyAngularVelocity,Vector3.new(1,2,3));same(p.RotVelocity,Vector3.new(1,2,3));same(position.Position,Vector3.new(10,20,30))
 assert(force.Magnitude==8.5 and velocity.PlaneVelocity==Vector2.new(8,12) and velocity.MaxPlanarAxesForce==Vector2.new(32,48))
"#);
}
#[test]
fn received_unknown_scale_or_missing_geometry_remains_unavailable() {
    for known in [false, true] {
        let mut properties = json!({"PrimaryPart":null,"Archivable":true,"WorldPivot":{"$cframe":{"position":[0,0,0],"rotation":[1,0,0,0,1,0,0,0,1]}}});
        if known {
            properties["Scale"] = json!(1);
        }
        let mut runtime=Runtime::new(json!({"version":1,"sequence":1,"services":{"Workspace":"workspace"},"nodes":[
            {"id":"workspace","class_name":"Workspace","name":"Workspace","parent":null,"properties":{}},
            {"id":"model","class_name":"Model","name":"Received","parent":"workspace","properties":properties,"server_owned":true},
            {"id":"part","class_name":"Part","name":"Part","parent":"model","properties":{"Archivable":true,"CFrame":{"$cframe":{"position":[4,6,8],"rotation":[1,0,0,0,1,0,0,0,1]}}},"server_owned":true}
        ]}),32).unwrap();
        execute(
            &mut runtime,
            if known {
                r#"
            local m=workspace.Received;local before=m.Part.CFrame
            assert(not pcall(function()m:ScaleTo(2)end));assert(m:GetScale()==1 and m.Part.CFrame==before)
        "#
            } else {
                r#"
            local m=workspace.Received;assert(not pcall(function()return m:GetScale()end))
            local c=m:Clone();assert(not pcall(function()return c:GetScale()end));assert(not pcall(function()c:ScaleTo(2)end))
        "#
            },
        );
    }
}

fn lua_value(value: &Value, kind: &str) -> String {
    match kind {
        "number" => value.to_string(),
        "Vector3" | "Vector2" | "CFrame" => format!(
            "{kind}.new({})",
            value
                .as_array()
                .unwrap()
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join(",")
        ),
        _ => panic!("unsupported reference type {kind}"),
    }
}
#[test]
fn saved_studio_745_physics_reference_matches_all_three_scales() {
    let reference: Value = serde_json::from_str(include_str!(
        "fixtures/model-scaling/model_scaling_studio745.json"
    ))
    .unwrap();
    assert_eq!(reference["version"], "0.745.0.7451137");
    let mut checks = 0;
    for case in reference["cases"].as_array().unwrap() {
        let class = case["class"].as_str().unwrap();
        // The Part capture includes read-only assembly-derived fields, covered
        // by the mass, bounds and linear/angular velocity regressions instead.
        if class == "Part" {
            continue;
        }
        let empty = serde_json::Map::new();
        let properties = case["properties"].as_object().unwrap_or(&empty);
        let mut source=format!("{SETUP}\nm.WorldPivot=CFrame.new();local p=part();local object=Instance.new({class:?},m)\n");
        if class == "Humanoid" {
            source.push_str("object.AutomaticScalingEnabled=false\n");
        }
        for (name, p) in properties {
            source.push_str(&format!(
                "object[{name:?}]={}\n",
                lua_value(&p["before"], p["type"].as_str().unwrap())
            ));
        }
        for (factor, key) in [(2., "at_2"), (0.5, "at_0.5"), (1., "at_1")] {
            source.push_str(&format!("m:ScaleTo({factor})\n"));
            for (name, p) in properties {
                let kind = p["type"].as_str().unwrap();
                let expected = lua_value(&p[key], kind);
                if kind == "Vector2" {
                    source.push_str(&format!("near(object[{name:?}].X,({expected}).X,.001);near(object[{name:?}].Y,({expected}).Y,.001)\n"));
                } else {
                    source.push_str(&format!(
                        "same(object[{name:?}],{expected},.001) -- {class}.{name} at {factor}\n"
                    ));
                }
                checks += 1;
            }
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            execute(&mut runtime(), &source)
        }));
        assert!(result.is_ok(), "Studio case {class} failed");
    }
    assert!(checks >= 450, "{checks} checks");
}
