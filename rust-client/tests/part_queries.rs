//! Occupied-volume queries through the real Luau VM, on macOS and Windows CI.
#![cfg(feature = "rust-host")]

use headless_luau::runtime_owner::Runtime;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

fn snapshot(nodes: Vec<Value>) -> Value {
    let mut all = vec![
        json!({"id":"workspace","class_name":"Workspace","name":"Workspace","parent":null,"properties":{}}),
    ];
    all.extend(nodes);
    json!({"version":1,"sequence":1,"services":{"Workspace":"workspace"},"nodes":all})
}
fn runtime() -> Runtime {
    Runtime::new(snapshot(vec![]), 32).unwrap()
}
fn execute(runtime: &mut Runtime, source: &str) -> Value {
    let first = runtime
        .call(json!({"op":"execute","source":source,"timeout_ms":2000}))
        .unwrap();
    let id = first["job_id"]
        .as_u64()
        .unwrap_or_else(|| panic!("{first}"));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let result = runtime.status(id).unwrap();
        assert_eq!(result["ok"], true, "{result}");
        if result["completed"] == true {
            return result;
        }
        assert!(
            Instant::now() < deadline,
            "spatial query job stalled: {result}"
        );
        runtime.poll().unwrap();
        std::thread::sleep(Duration::from_millis(1));
    }
}
const SETUP: &str = r#"
local world = Instance.new("WorldModel", workspace)
local function part(pos, size, shape, class)
    local p = Instance.new(class or "Part")
    p.CFrame = CFrame.new(pos)
    p.Size = size or Vector3.new(2,2,2)
    if shape then p.Shape = shape end
    p.Anchored = true
    p.Parent = world
    return p
end
local function near(a,b,eps) assert(math.abs(a-b)<(eps or 0.0001), tostring(a).." ~= "..tostring(b)) end
local function has(list,p) return table.find(list,p) ~= nil end
"#;
fn run(source: &str) {
    execute(&mut runtime(), &format!("{SETUP}\n{source}"));
}

#[test]
fn box_sweep_returns_contact_travel_and_normal() {
    run(r#"
    local source=part(Vector3.zero)
    local target=part(Vector3.new(10,0,0))
    local h=world:Shapecast(source,Vector3.new(20,0,0))
    assert(typeof(h)=="RaycastResult" and h.Instance==target)
    near(h.Distance,8); near(h.Position.X,9)
    near(h.Normal.X,-1); near(h.Normal.Y,0)
    assert(h.Material==Enum.Material.Plastic)
    assert(not pcall(function() h.Distance=0 end))
    assert(world:Shapecast(source,Vector3.new(-20,0,0))==nil)
    assert(world:Shapecast(source,Vector3.zero)==nil)
    "#);
}

#[test]
fn sphere_and_rotated_cylinder_sweeps_use_the_actual_shape() {
    run(r#"
    local source=part(Vector3.zero,Vector3.new(2,2,2),Enum.PartType.Ball)
    local target=part(Vector3.new(10,0,0),Vector3.new(2,2,2),Enum.PartType.Ball)
    near(world:Shapecast(source,Vector3.new(20,0,0)).Distance,8)
    source.Shape=Enum.PartType.Cylinder
    source.Size=Vector3.new(6,2,2)
    source.CFrame=CFrame.Angles(0,0,math.pi/2)
    target.Shape=Enum.PartType.Block
    near(world:Shapecast(source,Vector3.new(20,0,0)).Distance,8)
    source.CFrame=CFrame.new()
    near(world:Shapecast(source,Vector3.new(20,0,0)).Distance,6)
    "#);
}

#[test]
fn overlaps_reject_empty_sphere_and_wedge_bounds() {
    run(r#"
    local source=part(Vector3.zero,Vector3.new(4,4,4),Enum.PartType.Ball)
    local corner=part(Vector3.new(1.8,1.8,1.8),Vector3.new(.2,.2,.2))
    assert(has(world:GetPartBoundsInBox(source.CFrame,source.Size),corner))
    assert(not has(world:GetPartsInPart(source),corner))
    corner.CFrame=CFrame.new(1.8,0,0)
    assert(has(world:GetPartsInPart(source),corner))
    source:Destroy()
    source=part(Vector3.zero,Vector3.new(4,4,4),nil,"WedgePart")
    corner.CFrame=CFrame.new(0,1.8,-1.8)
    assert(not has(world:GetPartsInPart(source),corner))
    corner.CFrame=CFrame.new(0,-1.8,1.8)
    assert(has(world:GetPartsInPart(source),corner))
    "#);
}

#[test]
fn initial_overlaps_are_ignored_and_the_source_is_always_excluded() {
    run(r#"
    local source=part(Vector3.zero)
    local overlap=part(Vector3.zero,Vector3.new(6,6,6))
    local touching=part(Vector3.new(2,0,0))
    local target=part(Vector3.new(10,0,0))
    local h=world:Shapecast(source,Vector3.new(20,0,0))
    assert(h.Instance==target); near(h.Distance,8)
    local list=world:GetPartsInPart(source)
    assert(not has(list,source) and has(list,overlap) and has(list,touching))
    "#);
}

#[test]
fn hypothetical_pose_size_and_flattening_do_not_mutate_instances() {
    run(r#"
    local source=part(Vector3.zero)
    local target=part(Vector3.new(10,0,0))
    local beforePose,beforeSize=source.CFrame,source.Size
    local changed=0
    source.Changed:Connect(function() changed+=1 end)
    local h=world:Shapecast(source,Vector3.new(20,0,0),nil,CFrame.new(3,0,0),Vector3.new(4,2,2))
    near(h.Distance,4)
    h=world:Shapecast(source,Vector3.new(20,0,0),nil,nil,Vector3.zero)
    near(h.Distance,9)
    task.wait()
    assert(source.CFrame==beforePose and source.Size==beforeSize and changed==0)
    "#);
}

#[test]
fn margins_and_size_gradients_change_first_contact_analytically() {
    run(r#"
    local source=part(Vector3.zero)
    part(Vector3.new(10,0,0))
    local p=RaycastParams.new()
    p.Margin=.5
    near(world:Shapecast(source,Vector3.new(20,0,0),p).Distance,7.5)
    p.SizeGradient=1
    near(world:Shapecast(source,Vector3.new(20,0,0),p).Distance,7.5/1.05)
    p.Margin=0; p.SizeGradient=-1
    near(world:Shapecast(source,Vector3.new(20,0,0),p).Distance,8/.95)
    "#);
}

#[test]
fn overlap_tolerance_max_parts_and_filters_work_together() {
    run(r#"
    local source=part(Vector3.zero)
    local a=part(Vector3.new(2.02,0,0))
    local b=part(Vector3.new(0,0,0))
    local p=OverlapParams.new()
    assert(not has(world:GetPartsInPart(source,p),a))
    p.Tolerance=.03
    assert(has(world:GetPartsInPart(source,p),a))
    p.MaxParts=1
    assert(#world:GetPartsInPart(source,p)==1)
    p.MaxParts=0; p.IncludeInstances={a,b}; p.ExcludeInstances={b}
    local list=world:GetPartsInPart(source,p)
    assert(#list==1 and list[1]==a)
    p.IncludeInstances={}
    assert(#world:GetPartsInPart(source,p)==0)
    p.IncludeInstances=nil; p.ExcludeInstances={}
    p.FilterType=Enum.RaycastFilterType.Include
    p.FilterDescendantsInstances={b}
    assert(#world:GetPartsInPart(source,p)==1 and world:GetPartsInPart(source,p)[1]==b)
    "#);
}

#[test]
fn collide_query_and_brute_force_flags_apply_to_both_queries() {
    run(r#"
    local source=part(Vector3.zero)
    local target=part(Vector3.new(10,0,0))
    target.CanCollide=false; target.CanQuery=false
    local p=RaycastParams.new()
    assert(world:Shapecast(source,Vector3.new(20,0,0),p)==nil)
    p.BruteForceAllSlow=true
    assert(world:Shapecast(source,Vector3.new(20,0,0),p).Instance==target)
    p.BruteForceAllSlow=false; target.CanQuery=true
    assert(world:Shapecast(source,Vector3.new(20,0,0),p).Instance==target)
    p.RespectCanCollide=true
    assert(world:Shapecast(source,Vector3.new(20,0,0),p)==nil)
    target.CFrame=CFrame.new()
    local o=OverlapParams.new()
    o.RespectCanCollide=true
    assert(#world:GetPartsInPart(source,o)==0)
    o.BruteForceAllSlow=true
    assert(has(world:GetPartsInPart(source,o),target))
    o.ExcludeInstances={target}
    assert(#world:GetPartsInPart(source,o)==0)
    "#);
}

#[test]
fn nested_worlds_are_isolated_but_sweep_templates_can_be_unparented() {
    run(r#"
    local source=part(Vector3.zero)
    local target=part(Vector3.new(10,0,0))
    assert(workspace:Shapecast(source,Vector3.new(20,0,0))==nil)
    assert(world:Shapecast(source,Vector3.new(20,0,0)).Instance==target)
    assert(not pcall(function() workspace:GetPartsInPart(source) end))
    source.Parent=nil
    assert(world:Shapecast(source,Vector3.new(20,0,0)).Instance==target)
    assert(not pcall(function() world:GetPartsInPart(source) end))
    source.Parent=world
    local nested=Instance.new("WorldModel",world)
    target.Parent=nested
    assert(world:Shapecast(source,Vector3.new(20,0,0))==nil)
    local p=RaycastParams.new(); p.ExcludeInstances={source}
    assert(world:Raycast(Vector3.zero,Vector3.new(20,0,0),p)==nil)
    "#);
}

#[test]
fn invalid_arguments_and_missing_meshes_report_errors() {
    run(r#"
    local source=part(Vector3.zero)
    local bad={
        function() world:Shapecast(nil,Vector3.xAxis) end,
        function() world:Shapecast(source,Vector3.new(1025,0,0)) end,
        function() world:Shapecast(source,Vector3.new(0/0,0,0)) end,
        function() world:Shapecast(source,Vector3.xAxis,OverlapParams.new()) end,
        function() world:Shapecast(source,Vector3.xAxis,nil,nil,Vector3.new(-1,1,1)) end,
        function() world:Shapecast(source,Vector3.xAxis,nil,nil,Vector3.new(513,1,1)) end,
        function() world:Shapecast(source,Vector3.xAxis,nil,Vector3.zero) end,
        function() world:GetPartsInPart(source,RaycastParams.new()) end,
    }
    for i,f in bad do assert(not pcall(f),"accepted invalid case "..i) end
    local mesh=part(Vector3.new(10,0,0),nil,nil,"MeshPart")
    local ok,e=pcall(function() world:Shapecast(source,Vector3.new(20,0,0)) end)
    assert(not ok and string.find(e,"collision geometry"))
    mesh:Destroy(); source:Destroy()
    assert(not pcall(function() world:Shapecast(source,Vector3.xAxis) end))
    "#);
}

fn node(id: &str, class: &str, position: [f64; 3], size: [f64; 3]) -> Value {
    json!({"id":id,"class_name":class,"name":id,"parent":"workspace","properties":{
        "CFrame":{"$cframe":{"position":position,"rotation":[1,0,0,0,1,0,0,0,1]}},
        "Size":{"$vector3":size},"Shape":{"$enum":{"type":"PartType","name":"Block","value":1}},
        "Material":{"$enum":{"type":"Material","name":"Plastic","value":256}},
        "CanCollide":true,"CanQuery":true,"Anchored":true,"CollisionGroup":"Default"
    }})
}

#[test]
fn received_creates_moves_reparents_and_removes_refresh_all_query_catalogs() {
    let mut r = runtime();
    execute(
        &mut r,
        r#"
        source=Instance.new("Part",workspace); source.Size=Vector3.new(2,2,2); source.CFrame=CFrame.new()
        assert(workspace:Shapecast(source,Vector3.new(20,0,0))==nil)
    "#,
    );
    let update = |r: &mut Runtime, sequence: u64, actions: Vec<Value>| {
        let result=r.call(json!({"op":"update","delta":{"version":1,"delta":true,"sequence":sequence,"actions":actions}})).unwrap();
        assert_eq!(result["ok"], true, "{result}");
    };
    update(
        &mut r,
        2,
        vec![
            json!({"sequence":2,"kind":"create","node":node("received","Part",[10.,0.,0.],[2.;3])}),
        ],
    );
    execute(
        &mut r,
        r#"
        target=workspace:FindFirstChild("received"); assert(target)
        assert(workspace:Shapecast(source,Vector3.new(20,0,0)).Instance==target)
        local p=RaycastParams.new(); p.ExcludeInstances={source}
        assert(workspace:Raycast(Vector3.zero,Vector3.new(20,0,0),p).Instance==target)
    "#,
    );
    update(
        &mut r,
        3,
        vec![
            json!({"sequence":3,"kind":"set","id":"received","fields":{"CFrame":{"$cframe":{"position":[0,0,0],"rotation":[1,0,0,0,1,0,0,0,1]}}}}),
        ],
    );
    execute(&mut r,"assert(table.find(workspace:GetPartsInPart(source),target)); assert(workspace:Shapecast(source,Vector3.new(20,0,0))==nil)");
    update(
        &mut r,
        4,
        vec![json!({"sequence":4,"kind":"set","id":"received","fields":{"Parent":null}})],
    );
    execute(&mut r, "assert(#workspace:GetPartsInPart(source)==0)");
    update(
        &mut r,
        5,
        vec![
            json!({"sequence":5,"kind":"set","id":"received","fields":{"Parent":{"$ref":"workspace"}}}),
        ],
    );
    execute(
        &mut r,
        "assert(table.find(workspace:GetPartsInPart(source),target))",
    );
    update(
        &mut r,
        6,
        vec![json!({"sequence":6,"kind":"remove","id":"received"})],
    );
    execute(&mut r, "assert(#workspace:GetPartsInPart(source)==0)");
}

fn cube(center: [f64; 3], half: f64) -> (Vec<[f64; 3]>, Vec<[usize; 3]>) {
    let vertices = [
        [-1., -1., -1.],
        [1., -1., -1.],
        [1., 1., -1.],
        [-1., 1., -1.],
        [-1., -1., 1.],
        [1., -1., 1.],
        [1., 1., 1.],
        [-1., 1., 1.],
    ]
    .map(|v| std::array::from_fn(|i| center[i] + v[i] * half))
    .to_vec();
    let faces = vec![
        [0, 2, 1],
        [0, 3, 2],
        [4, 5, 6],
        [4, 6, 7],
        [0, 1, 5],
        [0, 5, 4],
        [3, 7, 6],
        [3, 6, 2],
        [0, 4, 7],
        [0, 7, 3],
        [1, 2, 6],
        [1, 6, 5],
    ];
    (vertices, faces)
}
fn mesh_node(
    id: &str,
    center: [f64; 3],
    size: [f64; 3],
    vertices: Vec<[f64; 3]>,
    faces: Vec<[usize; 3]>,
) -> Value {
    let mut mesh = node(id, "MeshPart", center, size);
    mesh["properties"]["CollisionGeometry"] =
        json!({"kind":"triangle_mesh","vertices":vertices,"triangles":faces});
    mesh
}
const SOURCE: &str = r#"
    local source=Instance.new("Part",workspace)
    source.Size=Vector3.new(2,2,2); source.CFrame=CFrame.new(); source.Anchored=true
    local mesh=workspace:FindFirstChild("mesh")
    local function near(a,b) assert(math.abs(a-b)<0.0001,tostring(a).." ~= "..tostring(b)) end
"#;
fn run_mesh(mesh: Value, source: &str) {
    let mut r = Runtime::new(snapshot(vec![mesh]), 32).unwrap();
    execute(&mut r, &format!("{SOURCE}\n{source}"));
}

#[test]
fn closed_mesh_containment_ignores_unreferenced_vertices() {
    let (mut v, f) = cube([10., 0., 0.], 4.);
    v.push([0., 0., 0.]); // This unused vertex must not manufacture an overlap.
    run_mesh(
        mesh_node("mesh", [10., 0., 0.], [8.; 3], v, f),
        r#"
        assert(#workspace:GetPartsInPart(source)==0)
        local h=workspace:Shapecast(source,Vector3.new(20,0,0))
        assert(h.Instance==mesh); near(h.Distance,5); near(h.Position.X,6)
        source.CFrame=CFrame.new(10,0,0)
        assert(table.find(workspace:GetPartsInPart(source),mesh))
        assert(workspace:Shapecast(source,Vector3.new(20,0,0))==nil)
    "#,
    );
}

#[test]
fn hollow_mesh_keeps_its_cavity_empty_and_reports_inner_wall() {
    let (mut v, mut f) = cube([0.; 3], 5.);
    let (inner, faces) = cube([0.; 3], 3.);
    let offset = v.len();
    v.extend(inner);
    f.extend(faces.into_iter().map(|face| face.map(|i| i + offset)));
    run_mesh(
        mesh_node("mesh", [0.; 3], [10.; 3], v, f),
        r#"
        assert(#workspace:GetPartsInPart(source)==0)
        assert(table.find(workspace:GetPartBoundsInBox(source.CFrame,source.Size),mesh))
        local h=workspace:Shapecast(source,Vector3.new(10,0,0))
        assert(h.Instance==mesh); near(h.Distance,2); near(h.Position.X,3)
        source.CFrame=CFrame.new(4,0,0)
        assert(table.find(workspace:GetPartsInPart(source),mesh))
        -- Already inside the material: ignore every later face of this mesh.
        assert(workspace:Shapecast(source,Vector3.new(-10,0,0))==nil)
    "#,
    );
}

#[test]
fn mesh_source_can_be_swept_scaled_rotated_and_flattened() {
    let (v, f) = cube([10., 0., 0.], 1.);
    run_mesh(
        mesh_node("mesh", [10., 0., 0.], [2.; 3], v, f),
        r#"
        local h=workspace:Shapecast(mesh,Vector3.new(-20,0,0))
        assert(h.Instance==source); near(h.Distance,8)
        h=workspace:Shapecast(mesh,Vector3.new(-20,0,0),nil,nil,Vector3.new(4,2,2))
        near(h.Distance,7)
        h=workspace:Shapecast(mesh,Vector3.new(-20,0,0),nil,CFrame.new(10,0,0)*CFrame.Angles(0,0,math.pi/2),Vector3.new(4,2,2))
        near(h.Distance,8)
        h=workspace:Shapecast(mesh,Vector3.new(-20,0,0),nil,nil,Vector3.zero)
        near(h.Distance,9)
        assert(mesh.Size==Vector3.new(2,2,2) and mesh.CFrame.Position==Vector3.new(10,0,0))
        mesh.Size=Vector3.new(4,2,2); mesh.CFrame=CFrame.new(12,0,0)
        near(workspace:Shapecast(source,Vector3.new(20,0,0)).Distance,9)
    "#,
    );
}

#[test]
fn open_meshes_remain_surfaces_and_can_supply_contact_material() {
    let mut mesh = mesh_node(
        "mesh",
        [10., 0., 0.],
        [2., 8., 8.],
        vec![[10., -4., -4.], [10., 4., -4.], [10., 0., 4.]],
        vec![[0, 1, 2]],
    );
    mesh["properties"]
        .as_object_mut()
        .unwrap()
        .remove("Material");
    mesh["properties"]["CollisionGeometry"]["material"] =
        json!({"$enum":{"type":"Material","value":1088}});
    run_mesh(
        mesh,
        r#"
        local h=workspace:Shapecast(source,Vector3.new(20,0,0))
        near(h.Distance,9); assert(h.Material==Enum.Material.Metal)
        source.CFrame=CFrame.new(10,0,0)
        assert(table.find(workspace:GetPartsInPart(source),mesh))
        source.CFrame=CFrame.new(12,0,0)
        assert(#workspace:GetPartsInPart(source)==0)
    "#,
    );
}

#[test]
fn disconnected_mesh_components_do_not_fill_the_gap_between_them() {
    let (mut v, mut f) = cube([0., -4., 0.], 1.);
    let (extra, faces) = cube([0., 4., 0.], 1.);
    let offset = v.len();
    v.extend(extra);
    f.extend(faces.into_iter().map(|face| face.map(|i| i + offset)));
    run_mesh(
        mesh_node("mesh", [0.; 3], [2., 10., 2.], v, f),
        r#"
        assert(#workspace:GetPartsInPart(source)==0)
        assert(workspace:Shapecast(source,Vector3.new(20,0,0))==nil)
        near(workspace:Shapecast(source,Vector3.new(0,10,0)).Distance,2)
    "#,
    );
}

#[test]
fn voxel_terrain_sweeps_report_material_and_respect_ignore_water() {
    let terrain = json!({"id":"terrain","class_name":"Terrain","name":"Terrain","parent":"workspace","properties":{
        "CollisionGeometry":{"kind":"voxel_grid","origin":[10,-1,-1],"cell_size":[2,2,2],"dimensions":[2,1,1],"occupancy":[true,true],
        "materials":[2048,256]}
    }});
    let mut r = Runtime::new(snapshot(vec![terrain]), 32).unwrap();
    execute(
        &mut r,
        &format!(
            "{SOURCE}\n{}",
            r#"
        local h=workspace:Shapecast(source,Vector3.new(20,0,0))
        assert(h.Instance==workspace.Terrain); near(h.Distance,9)
        assert(h.Material==Enum.Material.Water)
        local p=RaycastParams.new(); p.IgnoreWater=true
        h=workspace:Shapecast(source,Vector3.new(20,0,0),p)
        near(h.Distance,11); assert(h.Material==Enum.Material.Plastic)
        p.ExcludeInstances={workspace.Terrain}
        assert(workspace:Shapecast(source,Vector3.new(20,0,0),p)==nil)
        assert(not pcall(function() workspace:Shapecast(workspace.Terrain,Vector3.xAxis) end))
    "#
        ),
    );
}

#[test]
fn invalid_mesh_faces_fail_instead_of_becoming_solid_boxes() {
    for (v, f) in [
        (
            vec![[10., 0., 0.], [10., 1., 0.], [10., 0., 1.]],
            vec![[0, 1, 3]],
        ),
        (
            vec![[10., 0., 0.], [10., 1., 0.], [10., 2., 0.]],
            vec![[0, 1, 2]],
        ),
    ] {
        run_mesh(
            mesh_node("mesh", [10., 0., 0.], [2.; 3], v, f),
            r#"
            local ok,e=pcall(function() workspace:Shapecast(source,Vector3.new(20,0,0)) end)
            assert(not ok and string.find(e,"triangle"))
        "#,
        );
    }
}

#[test]
fn collision_group_matrix_filters_overlap_and_sweep_candidates() {
    let mut s = snapshot(vec![]);
    s["nodes"][0]["properties"]["CollisionGroups"] = json!("Default^0^1\\Ghost^1^2");
    let mut r = Runtime::new(s, 32).unwrap();
    execute(
        &mut r,
        r#"
        local source=Instance.new("Part",workspace); source.Size=Vector3.new(2,2,2); source.CFrame=CFrame.new()
        local target=Instance.new("Part",workspace); target.Size=source.Size; target.CFrame=CFrame.new(10,0,0)
        target.CollisionGroup="Ghost"
        local p=RaycastParams.new()
        assert(workspace:Shapecast(source,Vector3.new(20,0,0),p)==nil)
        p.CollisionGroup="Ghost"
        assert(workspace:Shapecast(source,Vector3.new(20,0,0),p).Instance==target)
        target.CFrame=CFrame.new()
        local o=OverlapParams.new()
        assert(#workspace:GetPartsInPart(source,o)==0)
        o.CollisionGroup="Ghost"
        assert(table.find(workspace:GetPartsInPart(source,o),target))
    "#,
    );
}

#[test]
fn oblique_sphere_sweeps_match_quadratic_contact_times() {
    run(r#"
        local source=part(Vector3.zero,Vector3.new(2,2,2),Enum.PartType.Ball)
        local target=part(Vector3.new(10,0,0),Vector3.new(2,2,2),Enum.PartType.Ball)
        local p=RaycastParams.new()
        for _,gradient in {-.8,0,2} do
            p.SizeGradient=gradient
            for i=0,12 do
                local y=i*.12
                target.CFrame=CFrame.new(10,y,0)
                local k=gradient/20
                local a,b,c=1-k*k,-20-4*k,96+y*y
                local expected=(-b-math.sqrt(b*b-4*a*c))/(2*a)
                local hit=world:Shapecast(source,Vector3.new(20,0,0),p)
                assert(hit and hit.Instance==target)
                near(hit.Distance,expected,.0001)
                near(hit.Normal.Magnitude,1,.0001)
                near((hit.Position-target.Position).Magnitude,1,.0001)
            end
        end
    "#);
}

#[test]
fn rotated_box_sweeps_match_support_plane_distance() {
    run(r#"
        local source=part(Vector3.zero,Vector3.new(4,2,2))
        part(Vector3.new(10,0,0),Vector3.new(2,40,40))
        for i=0,24 do
            local angle=i*math.pi/12
            source.CFrame=CFrame.Angles(0,0,angle)
            local expected=9-2*math.abs(math.cos(angle))-math.abs(math.sin(angle))
            near(world:Shapecast(source,Vector3.new(20,0,0)).Distance,expected)
        end
    "#);
}

#[test]
fn excessive_mesh_work_returns_a_bounded_error() {
    // Parallel triangles with overlapping AABBs but separated support planes.
    // Repeating disconnected faces must not turn a query into unbounded work.
    let v = vec![[0., 0., 0.], [4., 0., 0.], [0., 4., 0.]];
    let target = vec![[3., 3., 0.], [4., 3., 0.], [3., 4., 0.]];
    let a = mesh_node("a", [0.; 3], [4.; 3], v, vec![[0, 1, 2]; 400]);
    let b = mesh_node("b", [0.; 3], [4.; 3], target, vec![[0, 1, 2]; 400]);
    let mut r = Runtime::new(snapshot(vec![a, b]), 32).unwrap();
    execute(
        &mut r,
        r#"
        local ok,e=pcall(function() workspace:GetPartsInPart(workspace.a) end)
        assert(not ok and string.find(e,"work limit"),tostring(e))
    "#,
    );
}

#[test]
fn shipped_spatial_smoke_runs_in_the_official_vm() {
    let fixture: Value =
        serde_json::from_str(include_str!("../../tests/fixtures/world_snapshot.json")).unwrap();
    let mut r = Runtime::new(fixture, 32).unwrap();
    let result = execute(
        &mut r,
        include_str!("../../examples/spatial-query-smoke.luau"),
    );
    assert_eq!(
        result["values"][0]["passed"].as_f64(),
        Some(12.),
        "{result}"
    );
}
