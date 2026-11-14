#![cfg(feature = "rust-host")]
use headless_luau::runtime_owner::Runtime;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

fn runtime(display: Value) -> Runtime {
    Runtime::new(
        json!({"version":1,"sequence":1,"display":display,
            "services":{"Workspace":"ws","GuiService":"gui"},"nodes":[
            {"id":"ws","class_name":"Workspace","name":"Workspace","parent":null,"properties":{}},
            {"id":"gui","class_name":"GuiService","name":"GuiService","parent":null,"properties":{}}
        ]}),
        32,
    )
    .unwrap()
}
fn execute(r: &mut Runtime, source: &str) -> Value {
    let reply = r
        .call(json!({"op":"execute","source":source,"timeout_ms":2000}))
        .unwrap();
    let id = reply["job_id"]
        .as_u64()
        .unwrap_or_else(|| panic!("{reply}"));
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let reply = r.status(id).unwrap();
        assert_eq!(reply["ok"], true, "{reply}");
        if reply["completed"] == true {
            return reply;
        }
        assert!(Instant::now() < deadline, "{reply}");
        r.poll().unwrap();
        std::thread::sleep(Duration::from_millis(1));
    }
}
const SETUP: &str = r#"
c=Instance.new('Camera');c.CFrame=CFrame.new();c.FieldOfView=90
c.Parent=Instance.new('Folder',workspace);workspace.CurrentCamera=c
function near(a,b,eps) assert(math.abs(a-b)<(eps or .0002),tostring(a)..' ~= '..tostring(b)) end
function vnear(a,b,eps) near(a.X,b.X,eps);near(a.Y,b.Y,eps);near(a.Z,b.Z,eps) end
"#;
fn setup() -> Runtime {
    let mut r = runtime(json!({"size":[800,600]}));
    execute(&mut r, SETUP);
    r
}
fn resize(r: &mut Runtime, sequence: u64, display: Value) {
    let reply=r.call(json!({"op":"update","delta":{"version":1,"delta":true,"sequence":sequence,"actions":[],"display":display}})).unwrap();
    assert_eq!(reply["ok"], true, "{reply}");
}

#[test]
fn projects_center_depth_edges_and_points_behind_the_camera() {
    let mut r = setup();
    execute(
        &mut r,
        r#"
        local p,on=c:WorldToViewportPoint(Vector3.new(0,0,-10));vnear(p,Vector3.new(400,300,10));assert(on)
        p,on=c:WorldToViewportPoint(Vector3.new(10,10,-10));vnear(p,Vector3.new(700,0,10));assert(on)
        p,on=c:WorldToViewportPoint(Vector3.new(10,10.0001,-10));assert(p.Y<0 and not on)
        p,on=c:WorldToViewportPoint(Vector3.new(20,0,-10));assert(p.X>800 and not on)
        p,on=c:WorldToViewportPoint(Vector3.new(0,0,10));assert(p.Z==-10 and not on)
        p,on=c:WorldToViewportPoint(Vector3.zero);assert(not on and p.X~=p.X)
    "#,
    );
}

#[test]
fn perspective_math_uses_camera_rotation_and_translation() {
    let mut r = setup();
    execute(
        &mut r,
        r#"
        c.CFrame=CFrame.new(10,20,30)*CFrame.Angles(.3,.5,.7)
        for _,p in {Vector3.new(0,0,-10),Vector3.new(2,3,-4),Vector3.new(-2,-3,-4)} do
            local q=c:WorldToViewportPoint(c.CFrame:PointToWorldSpace(p))
            near(q.X,400+300*p.X/-p.Z,.001);near(q.Y,300-300*p.Y/-p.Z,.001);near(q.Z,-p.Z,.001)
        end
        local ray=c:ViewportPointToRay(400,300)
        vnear(ray.Direction,c.CFrame.LookVector);vnear(ray.Origin,c.CFrame.Position+c.CFrame.LookVector*.5)
    "#,
    );
}

#[test]
fn screen_coordinates_use_core_insets_and_viewport_uses_device_insets() {
    let mut r = runtime(
        json!({"size":[1000,800],"device_insets":[10,20,30,40],"core_insets":[15,50,35,45]}),
    );
    execute(&mut r, SETUP);
    execute(
        &mut r,
        r#"
        assert(c.ViewportSize==Vector2.new(960,740))
        local a,visible=c:WorldToViewportPoint(Vector3.new(0,0,-10));assert(visible)
        local b,visible2=c:WorldToScreenPoint(Vector3.new(0,0,-10));assert(visible2)
        vnear(a,Vector3.new(490,380,10));vnear(b,Vector3.new(485,350,10))
        local first=c:ViewportPointToRay(a.X,a.Y);local second=c:ScreenPointToRay(b.X,b.Y)
        vnear(first.Direction,second.Direction);vnear(first.Origin,second.Origin)
        local inset=game:GetService('GuiService'):GetGuiInset();assert(inset==Vector2.new(15,50))
        local p,on=c:WorldToScreenPoint(Vector3.new(0,9.375,-10));assert(p.Y<0 and on)
    "#,
    );
}

#[test]
fn rays_start_on_the_near_plane_then_apply_depth_along_a_unit_direction() {
    let mut r = setup();
    execute(
        &mut r,
        r#"
        local center=c:ViewportPointToRay(400,300,3)
        assert(typeof(center)=='Ray');vnear(center.Direction,Vector3.new(0,0,-1));vnear(center.Origin,Vector3.new(0,0,-3.5))
        local edge=c:ViewportPointToRay(700,300)
        near(edge.Direction.Magnitude,1);vnear(edge.Origin,Vector3.new(.5,0,-.5))
        local further=c:ViewportPointToRay(700,300,2)
        vnear(further.Origin,edge.Origin+edge.Direction*2)
        local negative=c:ViewportPointToRay(700,300,-2)
        vnear(negative.Origin,edge.Origin-edge.Direction*2)
    "#,
    );
}

#[test]
fn projecting_a_point_on_each_ray_round_trips_across_the_surface() {
    let mut r = setup();
    execute(
        &mut r,
        r#"
        c.CFrame=CFrame.new(10,20,30)*CFrame.Angles(.2,-.4,.6)
        for x=0,800,100 do for y=0,600,100 do
            local ray=c:ViewportPointToRay(x,y,2)
            local p=c:WorldToViewportPoint(ray.Origin+ray.Direction*25)
            near(p.X,x,.003);near(p.Y,y,.003)
        end end
    "#,
    );
}

#[test]
fn inactive_cameras_retain_their_last_viewport_and_clones_start_with_one_pixel() {
    let mut r = setup();
    execute(&mut r,"other=Instance.new('Camera');other.CFrame=CFrame.new();other.FieldOfView=90;assert(other.ViewportSize==Vector2.one)");
    resize(&mut r, 2, json!({"size":[1600,900]}));
    execute(&mut r,"assert(c.ViewportSize==Vector2.new(1600,900));workspace.CurrentCamera=other;assert(other.ViewportSize==c.ViewportSize)");
    resize(&mut r, 3, json!({"size":[640,480]}));
    execute(
        &mut r,
        r#"
        assert(c.ViewportSize==Vector2.new(1600,900));assert(other.ViewportSize==Vector2.new(640,480))
        local clone=c:Clone();assert(clone.ViewportSize==Vector2.one)
        workspace.CurrentCamera=c;assert(c.ViewportSize==Vector2.new(640,480))
    "#,
    );
}

#[test]
fn viewport_callbacks_see_complete_lens_state_and_fire_once_per_resize() {
    let mut r = setup();
    execute(
        &mut r,
        r#"
        seen=0;diagonal_seen=0
        c:GetPropertyChangedSignal('ViewportSize'):Connect(function()
            seen+=1;assert(c.ViewportSize==Vector2.new(1000,500))
            near(math.tan(math.rad(c.DiagonalFieldOfView/2)),math.sqrt(5))
        end)
        c:GetPropertyChangedSignal('DiagonalFieldOfView'):Connect(function() diagonal_seen+=1 end)
    "#,
    );
    resize(&mut r, 2, json!({"size":[1000,500]}));
    execute(&mut r, "task.wait();assert(seen==1 and diagonal_seen==1)");
    resize(&mut r, 3, json!({"size":[1000,500]}));
    execute(&mut r, "task.wait();assert(seen==1 and diagonal_seen==1)");
}

#[test]
fn zero_area_display_detaches_gui_but_preserves_camera_geometry() {
    let mut r = setup();
    resize(&mut r, 2, json!({"size":[0,0]}));
    execute(&mut r,"assert(c.ViewportSize==Vector2.new(800,600));local p,on=c:WorldToViewportPoint(Vector3.new(0,0,-5));assert(on and p.X==400 and p.Y==300)");
}

#[test]
fn diagonal_and_max_axis_modes_keep_the_selected_angle_during_resize() {
    let mut r = setup();
    execute(
        &mut r,
        "c.FieldOfViewMode=Enum.FieldOfViewMode.Diagonal;diagonal=c.DiagonalFieldOfView",
    );
    resize(&mut r, 2, json!({"size":[1600,600]}));
    execute(
        &mut r,
        r#"
        near(c.DiagonalFieldOfView,diagonal)
        near(math.tan(math.rad(c.FieldOfView/2)),(1000/600)*600/math.sqrt(1600^2+600^2))
        c.FieldOfViewMode=Enum.FieldOfViewMode.MaxAxis;maximum=c.MaxAxisFieldOfView
    "#,
    );
    resize(&mut r, 3, json!({"size":[600,1600]}));
    execute(
        &mut r,
        "near(c.MaxAxisFieldOfView,maximum);near(c.FieldOfView,maximum)",
    );
}

#[test]
fn lens_setters_keep_angles_coherent_clamp_vertical_and_reject_invalid_input() {
    let mut r = setup();
    execute(
        &mut r,
        r#"
        c.DiagonalFieldOfView=90
        near(math.tan(math.rad(c.FieldOfView/2)),.6);near(c.DiagonalFieldOfView,90)
        c.MaxAxisFieldOfView=90;near(math.tan(math.rad(c.FieldOfView/2)),.75)
        c.FieldOfView=-5;assert(c.FieldOfView==1);c.FieldOfView=500;assert(c.FieldOfView==120)
        local before=c.FieldOfView
        assert(not pcall(function() c.FieldOfView=0/0 end));assert(c.FieldOfView==before)
        assert(not pcall(function() c.FieldOfViewMode=Enum.CameraType.Custom end))
        assert(not pcall(function() c.ViewportSize=Vector2.new(10,10) end))
        c.FieldOfViewMode=Enum.FieldOfViewMode.Diagonal;c:ResetPropertyToDefault('FieldOfViewMode')
        assert(c.FieldOfViewMode==Enum.FieldOfViewMode.Vertical)
    "#,
    );
}

#[test]
fn view_stretch_changes_horizontal_projection_and_ray_inversely() {
    let mut r = setup();
    execute(
        &mut r,
        r#"
        c.ViewStretch=2
        local p=c:WorldToViewportPoint(Vector3.new(1,0,-10));near(p.X,460)
        local ray=c:ViewportPointToRay(p.X,p.Y)
        near(ray.Direction.X/-ray.Direction.Z,.1)
    "#,
    );
}

#[test]
fn invalid_display_delta_rolls_back_its_world_actions() {
    let mut r = setup();
    execute(
        &mut r,
        "marker=Instance.new('Part',workspace);marker.Name='Marker'",
    );
    let invalid=r.call(json!({"op":"update","delta":{"version":1,"delta":true,"sequence":2,"display":{"size":[20,20],"device_insets":[30,0,0,0]},"actions":[{"sequence":2,"kind":"create","node":{"id":"bad","class_name":"Part","name":"ShouldNotExist","parent":"ws","properties":{}}}]}}));
    assert!(invalid.is_err() || invalid.unwrap()["ok"] == false);
    execute(&mut r,"assert(c.ViewportSize==Vector2.new(800,600));assert(workspace:FindFirstChild('ShouldNotExist')==nil)");
    resize(&mut r, 2, json!({"size":[640,480]}));
    execute(&mut r, "assert(c.ViewportSize==Vector2.new(640,480))");
}

#[test]
fn old_viewport_receipts_feed_camera_and_gui_the_same_surface() {
    let mut r = setup();
    let reply=r.call(json!({"op":"update","delta":{"version":1,"delta":true,"sequence":2,"actions":[],"viewport":{"size":[1000,800],"device_safe_insets":[10,20,30,40],"gui_inset":[5,30]}}})).unwrap();
    assert_eq!(reply["ok"], true, "{reply}");
    execute(
        &mut r,
        r#"
        assert(c.ViewportSize==Vector2.new(960,740))
        assert(game:GetService('GuiService'):GetGuiInset()==Vector2.new(15,50))
        vnear(c:WorldToScreenPoint(Vector3.new(0,0,-5)),Vector3.new(485,350,5))
    "#,
    );
}

fn imported() -> Runtime {
    Runtime::new(json!({"version":1,"sequence":1,"display":{"size":[800,600]},"services":{"Workspace":"ws"},"nodes":[
        {"id":"ws","class_name":"Workspace","name":"Workspace","parent":null,"properties":{"CurrentCamera":{"$ref":"camera"}}},
        {"id":"camera","class_name":"Camera","name":"Camera","parent":"ws","properties":{"CFrame":{"$cframe":{"position":[0,0,0],"rotation":[1,0,0,0,1,0,0,0,1]}},"FieldOfView":90,"FieldOfViewMode":{"$enum":{"type":"FieldOfViewMode","name":"Diagonal","value":1}},"ViewStretch":1,"NearPlaneZ":-0.25}}
    ]}),32).unwrap()
}

#[test]
fn imported_camera_uses_received_lens_without_reinterpreting_initial_viewport() {
    let mut r = imported();
    execute(&mut r,"c=workspace.CurrentCamera;assert(c.FieldOfView==90);local ray=c:ViewportPointToRay(400,300);assert(ray.Origin.Z==-.25)");
    resize(&mut r, 2, json!({"size":[1600,600]}));
    execute(&mut r, "assert(c.FieldOfView<90)");
}

#[test]
fn received_fov_and_resize_in_one_delta_do_not_apply_the_aspect_change_twice() {
    let mut r = imported();
    let reply=r.call(json!({"op":"update","delta":{"version":1,"delta":true,"sequence":2,"display":{"size":[1600,600]},"actions":[{"sequence":2,"kind":"set","id":"camera","fields":{"FieldOfView":60}}]}})).unwrap();
    assert_eq!(reply["ok"], true, "{reply}");
    execute(&mut r,"assert(workspace.CurrentCamera.FieldOfView==60);assert(workspace.CurrentCamera.ViewportSize==Vector2.new(1600,600))");
}

#[test]
fn missing_received_lens_or_pose_does_not_turn_into_constructor_data() {
    let mut r = Runtime::new(
        json!({"version":1,"sequence":1,"services":{"Workspace":"ws"},"nodes":[
            {"id":"ws","class_name":"Workspace","name":"Workspace","parent":null,"properties":{}},
            {"id":"unknown","class_name":"Camera","name":"Unknown","parent":"ws","properties":{}}
        ]}),
        32,
    )
    .unwrap();
    execute(&mut r,"assert(not pcall(function() workspace.Unknown:WorldToViewportPoint(Vector3.zero) end));assert(not pcall(function() workspace.Unknown:ViewportPointToRay(0,0) end))");
}

#[test]
fn malformed_inputs_and_destroyed_receivers_fail_explicitly() {
    let mut r = setup();
    execute(
        &mut r,
        r#"
        assert(not pcall(function() c:WorldToViewportPoint(Vector3.new(0/0,0,0)) end))
        assert(not pcall(function() c:ViewportPointToRay(math.huge,0) end))
        assert(not pcall(function() c:ScreenPointToRay('3',0) end))
        assert(not pcall(function() workspace.CurrentCamera=Instance.new('Part') end))
        assert(workspace.CurrentCamera==c)
        c:Destroy();assert(not pcall(function() c:WorldToScreenPoint(Vector3.zero) end))
    "#,
    );
}

#[test]
fn camera_transform_aliases_commit_together_and_reject_invalid_rotations() {
    let mut r = setup();
    execute(
        &mut r,
        r#"
        local next=CFrame.new(3,4,5)*CFrame.Angles(.2,.3,.4)
        local seen=0
        c:GetPropertyChangedSignal('CFrame'):Connect(function()
            assert(c.CFrame==next and c.CoordinateFrame==next);seen+=1
        end)
        c.CoordinateFrame=next;task.wait();assert(seen==1)
        assert(not pcall(function() c.CFrame=CFrame.new(0/0,0,0) end));assert(c.CFrame==next)
        for _,bad in {CFrame.new(0,0,0,-1,0,0,0,1,0,0,0,1),CFrame.new(0,0,0,2,0,0,0,1,0,0,0,1)} do
            assert(not pcall(function() c.CFrame=bad end));assert(c.CFrame==next)
        end
        local p=Instance.new('Part');p.CFrame=CFrame.new(7,8,9);assert(p.Position==Vector3.new(7,8,9))
    "#,
    );
}

#[test]
fn received_lens_changes_publish_derived_events_once_without_resize() {
    let mut r = imported();
    execute(&mut r, "c=workspace.CurrentCamera;seen=0;diagonal_seen=0;c:GetPropertyChangedSignal('FieldOfView'):Connect(function() seen+=1 end);c:GetPropertyChangedSignal('DiagonalFieldOfView'):Connect(function() diagonal_seen+=1 end)");
    let reply=r.call(json!({"op":"update","delta":{"version":1,"delta":true,"sequence":2,"actions":[{"sequence":2,"kind":"set","id":"camera","fields":{"FieldOfView":60}}]}})).unwrap();
    assert_eq!(reply["ok"], true, "{reply}");
    execute(
        &mut r,
        "task.wait();assert(seen==1 and diagonal_seen==1);assert(c.FieldOfView==60)",
    );
}

#[test]
fn viewport_camera_inherits_projection_and_lens_bindings() {
    let mut r = setup();
    execute(&mut r, "local vc=Instance.new('ViewportCamera');vc.CFrame=CFrame.new();vc.FieldOfView=90;workspace.CurrentCamera=vc;assert(vc:IsA('Camera'));local p,on=vc:WorldToViewportPoint(Vector3.new(0,0,-5));assert(on and p==Vector3.new(400,300,5))");
}

#[test]
fn shipped_camera_smoke_runs_in_the_official_vm() {
    let snapshot = serde_json::from_str(include_str!(
        "../../tests/fixtures/camera_projection_snapshot.json"
    ))
    .unwrap();
    let mut r = Runtime::new(snapshot, 32).unwrap();
    let reply = execute(
        &mut r,
        include_str!("../../examples/camera-projection-smoke.luau"),
    );
    assert_eq!(reply["values"][0]["passed"].as_f64(), Some(16.0), "{reply}");
}
