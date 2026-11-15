use super::world::{World, WorldRef};
use super::{
    add3, box_support, boxes_overlap, cast_convex, closest_primitive, primitive_world_support,
    ray_box, ray_primitive, sphere_box, sweep_box_box, CFrameValue,
};
use serde_json::json;
use std::{cell::RefCell, rc::Rc};

fn identity() -> CFrameValue {
    CFrameValue::new(
        [0.0, 0.0, 0.0],
        [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
    )
}

#[test]
fn raycast_result_exposes_the_engine_type_name() {
    let lua = mlua::Lua::new();
    let hit = lua
        .create_userdata(super::Hit {
            world: std::rc::Weak::new(),
            id: "part".into(),
            position: [0.0, 0.0, 1.0],
            normal: [0.0, 0.0, -1.0],
            distance: 4.0,
            material: json!(256),
        })
        .unwrap();
    lua.globals().set("hit", hit).unwrap();
    assert_eq!(
        lua.load("return typeof(hit), hit.Distance")
            .eval::<(String, f64)>()
            .unwrap(),
        ("RaycastResult".into(), 4.0)
    );
}

#[test]
fn ray_hits_box_at_nearest_face_and_returns_normal() {
    let hit = ray_box(
        [0.0, 0.0, -5.0],
        [0.0, 0.0, 1.0],
        &identity(),
        [1.0; 3],
        10.0,
    )
    .unwrap();
    assert!((hit.0 - 4.0).abs() < 1e-9);
    assert_eq!(hit.1, [0.0, 0.0, -1.0]);
}

#[test]
fn ray_misses_when_parallel_outside_a_slab() {
    assert!(ray_box(
        [0.0, 2.0, -5.0],
        [0.0, 0.0, 1.0],
        &identity(),
        [1.0; 3],
        10.0
    )
    .is_none());
}

#[test]
fn ray_starting_inside_uses_exit_face() {
    let hit = ray_box([0.0; 3], [1.0, 0.0, 0.0], &identity(), [1.0; 3], 10.0).unwrap();
    assert!((hit.0 - 1.0).abs() < 1e-9);
    assert_eq!(hit.1, [1.0, 0.0, 0.0]);
}

#[test]
fn ray_respects_maximum_distance() {
    assert!(ray_box(
        [0.0, 0.0, -5.0],
        [0.0, 0.0, 1.0],
        &identity(),
        [1.0; 3],
        3.9
    )
    .is_none());
}

#[test]
fn ray_uses_part_rotation_for_oriented_box() {
    // 90 degrees around Y: the local X face is reached along world Z.
    let frame = CFrameValue::new(
        [0.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0, 1.0, 0.0, -1.0, 0.0, 0.0],
    );
    let hit = ray_box(
        [0.0, 0.0, -5.0],
        [0.0, 0.0, 1.0],
        &frame,
        [2.0, 1.0, 0.5],
        10.0,
    )
    .unwrap();
    assert!((hit.0 - 3.0).abs() < 1e-9);
    assert!((hit.1[2] + 1.0).abs() < 1e-9);
}

#[test]
fn ray_hits_ellipsoid_and_cylinder_sides() {
    let sphere = ray_primitive([0.0, 0.0, -5.0], [0.0, 0.0, 1.0], [1.0; 3], 0, 10.0).unwrap();
    assert!((sphere.0 - 4.0).abs() < 1e-9);
    assert_eq!(sphere.1, [0.0, 0.0, -1.0]);
    let cylinder =
        ray_primitive([0.0, 0.0, -5.0], [0.0, 0.0, 1.0], [2.0, 1.0, 1.0], 2, 10.0).unwrap();
    assert!((cylinder.0 - 4.0).abs() < 1e-9);
    assert_eq!(cylinder.1, [0.0, 0.0, -1.0]);
}

#[test]
fn ray_hits_wedge_and_corner_wedge_clipping_planes() {
    assert!(ray_primitive([0.0, 0.0, -5.0], [0.0, 0.0, 1.0], [1.0; 3], 3, 10.0).is_some());
    assert!(ray_primitive([0.0, 0.0, -5.0], [0.0, 0.0, 1.0], [1.0; 3], 4, 10.0).is_some());
}

#[test]
fn overlap_bounds_use_oriented_sat_and_sphere_distance() {
    let a = identity();
    let mut b = identity();
    b.position = [2.1, 0.0, 0.0];
    assert!(!boxes_overlap(&a, [1.0; 3], &b, [1.0; 3]));
    b.position = [1.9, 0.0, 0.0];
    assert!(boxes_overlap(&a, [1.0; 3], &b, [1.0; 3]));
    assert!(sphere_box([0.0, 0.0, 0.0], 0.0, &a, [1.0; 3]));
    assert!(!sphere_box([2.0, 2.0, 0.0], 1.0, &a, [1.0; 3]));
}

#[test]
fn closest_point_projects_to_ellipsoid_and_wedge_surface() {
    let ellipse = closest_primitive([3.0, 0.0, 0.0], [2.0, 1.0, 1.0], 0).unwrap();
    // Along the major axis the closest ellipsoid surface point is the tip.
    assert!((ellipse[0] - 2.0).abs() < 1e-9);
    assert_eq!(ellipse[1..], [0.0, 0.0]);
    let inside = closest_primitive([0.0; 3], [1.0; 3], 3).unwrap();
    assert_eq!(inside, [0.0; 3]);
    let wedge = closest_primitive([0.0, 2.0, -2.0], [1.0; 3], 3).unwrap();
    assert!(wedge.iter().all(|v| v.is_finite()));
}

#[test]
fn swept_oriented_box_reports_first_contact_and_surface_normal() {
    let start = identity();
    let mut target = identity();
    target.position = [5.0, 0.0, 0.0];
    let hit = sweep_box_box(&start, [1.0; 3], [1.0, 0.0, 0.0], 10.0, &target, [1.0; 3]).unwrap();
    assert!((hit.0 - 3.0).abs() < 1e-9);
    assert_eq!(hit.1, [-1.0, 0.0, 0.0]);
    assert!((hit.2[0] - 4.0).abs() < 1e-9);
    assert!(sweep_box_box(&start, [1.0; 3], [1.0, 0.0, 0.0], 2.99, &target, [1.0; 3]).is_none());
}

#[test]
fn gjk_block_sweep_hits_sphere_target_at_exact_axis_contact() {
    let moving = identity();
    let mut target = identity();
    target.position = [5.0, 0.0, 0.0];
    let moving_axes = super::axes(&moving);
    let hit = cast_convex(
        [1.0, 0.0, 0.0],
        10.0,
        |axis| add3(moving.position, box_support(moving_axes, [1.0; 3], axis)),
        |axis| primitive_world_support(&target, [1.0; 3], 0, axis),
        &mut 0,
    )
    .unwrap()
    .unwrap();
    assert!((hit.distance - 3.0).abs() < 1e-7);
    assert!((hit.point[0] - 4.0).abs() < 1e-7);
    assert!(hit.normal[0] < -0.99);
}

#[test]
fn gjk_sphere_sweep_hits_decoded_triangle_surface() {
    let triangle = [[5.0, -2.0, -2.0], [5.0, 2.0, -2.0], [5.0, 0.0, 2.0]];
    let origin = [0.0, 0.0, 0.0];
    let radius = 1.0;
    let hit = cast_convex(
        [1.0, 0.0, 0.0],
        10.0,
        |axis| add3(origin, super::scale3(axis, radius)),
        |axis| {
            *triangle
                .iter()
                .max_by(|a, b| super::dot3(**a, axis).total_cmp(&super::dot3(**b, axis)))
                .unwrap()
        },
        &mut 0,
    )
    .unwrap()
    .unwrap();
    assert!((hit.distance - 4.0).abs() < 1e-7);
    assert!((hit.point[0] - 5.0).abs() < 1e-7);
    assert!(hit.normal[0] < -0.99);
}

#[test]
fn gjk_sphere_sweep_hits_occupied_terrain_cell_and_skips_water() {
    let grid = json!({
        "origin":[0,0,0],
        "cell_size":[2,2,2],
        "dimensions":[1,1,1],
        "occupancy":[true],
        "materials":[{"$enum":{"type":"Material","value":256}}]
    });
    let origin = [-5.0, 1.0, 1.0];
    let hit = super::cast_voxels(
        &grid,
        [1.0, 0.0, 0.0],
        10.0,
        |axis| add3(origin, super::scale3(axis, 1.0)),
        false,
        &mut 0,
    )
    .unwrap()
    .unwrap();
    assert!((hit.0.distance - 4.0).abs() < 1e-7);
    assert_eq!(hit.1.unwrap()["$enum"]["value"], 256);

    let water = json!({
        "origin":[0,0,0],
        "cell_size":[2,2,2],
        "dimensions":[1,1,1],
        "occupancy":[true],
        "materials":[{"$enum":{"type":"Material","value":2048}}]
    });
    assert!(super::cast_voxels(
        &water,
        [1.0, 0.0, 0.0],
        10.0,
        |axis| add3(origin, super::scale3(axis, 1.0)),
        true,
        &mut 0,
    )
    .unwrap()
    .is_none());
}

fn assembly_snapshot() -> serde_json::Value {
    let frame = |x: i32| json!({"$cframe":{"position":[x,0,0],"rotation":[1,0,0,0,1,0,0,0,1]}});
    let part =
        |id: &str, name: &str, size: [i32; 3], x: i32, root_priority: i32, massless: bool| {
            json!({
                "id":id,
                "class_name":"Part",
                "name":name,
                "parent":"ws",
                "server_owned":false,
                "properties":{
                    "Size":{"$vector3":size},
                    "CFrame":frame(x),
                    "Shape":{"$enum":{"type":"PartType","value":1}},
                    "Material":{"$enum":{"type":"Material","value":256}},
                    "CustomPhysicalProperties":{"$physical_properties":[1.0,0.3,0.5,1.0,1.0]},
                    "Anchored":false,
                    "Massless":massless,
                    "RootPriority":root_priority,
                    "CanQuery":true,
                    "CanCollide":true
                }
            })
        };
    json!({
        "version":1,
        "sequence":0,
        "nodes":[
            {"id":"ws","class_name":"Workspace","name":"Workspace","parent":null},
            part("p1","root candidate",[2,2,2],0,0,false),
            part("p2","massless candidate",[1,1,1],4,10,true),
            part("p3","separate",[3,3,3],20,0,false),
            {
                "id":"weld",
                "class_name":"WeldConstraint",
                "name":"WeldConstraint",
                "parent":"ws",
                "server_owned":false,
                "properties":{"Part0":{"$ref":"p1"},"Part1":{"$ref":"p2"},"Enabled":true}
            }
        ]
    })
}

#[test]
fn assembly_queries_follow_connections_disconnection_and_massless_root_rules() {
    let snapshot = assembly_snapshot();
    let world = World::from_snapshot(&snapshot, None).unwrap();
    let world: WorldRef = Rc::new(RefCell::new(world));

    // The non-massless p1 wins before RootPriority is considered, so p2 is
    // excluded from the assembly total.
    let mass = super::mass_assembly_value(&world, "p1", "AssemblyMass").unwrap();
    assert_eq!(mass, json!(8.0));
    let center = super::mass_assembly_value(&world, "p1", "AssemblyCenterOfMass").unwrap();
    assert_eq!(center, json!({"$vector3":[0.0, 0.0, 0.0]}));

    // If every member is Massless, RootPriority picks p2. The root remains
    // included even though it is Massless; the other member is excluded.
    world
        .borrow_mut()
        .nodes
        .get_mut("p1")
        .unwrap()
        .properties
        .insert("Massless".into(), json!(true));
    assert_eq!(
        super::mass_assembly_value(&world, "p1", "AssemblyMass").unwrap(),
        json!(1.0)
    );
    assert_eq!(
        super::mass_assembly_value(&world, "p1", "AssemblyCenterOfMass").unwrap(),
        json!({"$vector3":[4.0, 0.0, 0.0]})
    );

    // Two contributing bodies use the mass-weighted center, independent of
    // which member supplied the property lookup.
    for id in ["p1", "p2"] {
        world
            .borrow_mut()
            .nodes
            .get_mut(id)
            .unwrap()
            .properties
            .insert("Massless".into(), json!(false));
    }
    assert_eq!(
        super::mass_assembly_value(&world, "p2", "AssemblyMass").unwrap(),
        json!(9.0)
    );
    let weighted = super::mass_assembly_value(&world, "p2", "AssemblyCenterOfMass").unwrap();
    assert!((weighted["$vector3"][0].as_f64().unwrap() - 4.0 / 9.0).abs() < 1e-6);

    // The unrelated p3 is its own assembly and the now-disabled joint no
    // longer contributes p2 to p1's assembly.
    assert_eq!(
        super::mass_assembly_value(&world, "p3", "AssemblyMass").unwrap(),
        json!(27.0)
    );
    world
        .borrow_mut()
        .nodes
        .get_mut("weld")
        .unwrap()
        .properties
        .insert("Enabled".into(), json!(false));
    assert_eq!(
        super::mass_assembly_value(&world, "p1", "AssemblyMass").unwrap(),
        json!(8.0)
    );
}

#[test]
fn assembly_query_reports_an_invalidated_joint_endpoint() {
    let snapshot = assembly_snapshot();
    let world = World::from_snapshot(&snapshot, None).unwrap();
    let world: WorldRef = Rc::new(RefCell::new(world));
    world
        .borrow_mut()
        .nodes
        .get_mut("weld")
        .unwrap()
        .properties
        .insert("Part1".into(), json!({"$ref":"missing-part"}));
    let error = super::mass_assembly_value(&world, "p1", "AssemblyMass")
        .unwrap_err()
        .to_string();
    assert!(error.contains("joint endpoint is unresolved"));
}

#[test]
fn anchored_assembly_has_infinite_mass_and_anchored_member_center() {
    let snapshot = assembly_snapshot();
    let world = World::from_snapshot(&snapshot, None).unwrap();
    let world: WorldRef = Rc::new(RefCell::new(world));
    world
        .borrow_mut()
        .nodes
        .get_mut("p2")
        .unwrap()
        .properties
        .insert("Anchored".into(), json!(true));
    assert_eq!(
        super::mass_assembly_value(&world, "p1", "AssemblyMass").unwrap(),
        json!({"$number":"inf"})
    );
    assert_eq!(
        super::mass_assembly_value(&world, "p1", "AssemblyCenterOfMass").unwrap(),
        json!({"$vector3":[4.0, 0.0, 0.0]})
    );
}

#[test]
fn instance_new_part_mass_uses_typed_shape_and_material_defaults() {
    let snapshot = json!({
        "version":1,
        "sequence":0,
        "services":{"Workspace":"ws"},
        "nodes":[
            {"id":"game","class_name":"DataModel","name":"game","parent":null},
            {"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game"}
        ]
    });
    let world = World::from_snapshot(&snapshot, None).unwrap();
    let world: WorldRef = Rc::new(RefCell::new(world));
    let lua = mlua::Lua::new();
    super::super::numeric::install(&lua).unwrap();
    super::super::enums::install(&lua).unwrap();
    super::super::signals::install(&lua, &world).unwrap();
    super::world::install(&lua, &world).unwrap();
    super::install(&lua, &world).unwrap();

    let result: bool = lua
        .load(
            r#"
        local part=Instance.new("Part", workspace)
        part.Size=Vector3.new(2,2,2)
        assert(part.Shape==Enum.PartType.Block)
        assert(part.Material==Enum.Material.Plastic)
        assert(math.abs(part:GetMass()-5.6)<1e-5)

        part.Shape=Enum.PartType.Ball
        assert(math.abs(part:GetMass()-(math.pi*8/6*0.7))<1e-5)
        part.Shape=Enum.PartType.Block
        part.Material=Enum.Material.Metal
        assert(math.abs(part:GetMass()-62.8)<1e-5)

        part:ResetPropertyToDefault("Shape")
        part:ResetPropertyToDefault("Material")
        assert(part.Shape==Enum.PartType.Block)
        assert(part.Material==Enum.Material.Plastic)
        assert(math.abs(part:GetMass()-5.6)<1e-5)
        return true
    "#,
        )
        .eval()
        .unwrap();
    assert!(result);
}

#[test]
fn voxel_ray_returns_cell_face_and_material_and_ignore_water_skips() {
    let grid = json!({"origin":[0,0,0],"cell_size":[2,2,2],"dimensions":[1,1,1],"occupancy":[true],"materials":[{"$enum":{"type":"Material","value":256}}]});
    let mut work = 0;
    let hit = super::ray_voxel_grid(
        &grid,
        [-5.0, 1.0, 1.0],
        [1.0, 0.0, 0.0],
        10.0,
        false,
        &mut work,
    )
    .unwrap()
    .unwrap();
    assert!((hit.0 - 5.0).abs() < 1e-9);
    assert_eq!(hit.1, [-1.0, 0.0, 0.0]);
    assert_eq!(hit.2["$enum"]["value"], 256);

    let water = json!({"origin":[0,0,0],"cell_size":[2,2,2],"dimensions":[1,1,1],"occupancy":[true],"materials":[{"$enum":{"type":"Material","value":2048}}]});
    assert!(super::ray_voxel_grid(
        &water,
        [-5.0, 1.0, 1.0],
        [1.0, 0.0, 0.0],
        10.0,
        true,
        &mut work
    )
    .unwrap()
    .is_none());
}

#[test]
fn mesh_vertices_follow_local_part_pose_and_size_edits() {
    let snapshot = json!({"version":1,"sequence":0,"nodes":[{"id":"ws","class_name":"Workspace","name":"Workspace","parent":null},{"id":"mesh","class_name":"MeshPart","name":"MeshPart","parent":"ws"}]});
    let mut world = World::from_snapshot(&snapshot, None).unwrap();
    let identity_cf = json!({"$cframe":{"position":[0,0,0],"rotation":[1,0,0,0,1,0,0,0,1]}});
    let node = world.nodes.get_mut("mesh").unwrap();
    node.properties.insert("CFrame".into(), identity_cf);
    node.properties
        .insert("Size".into(), json!({"$vector3":[2,2,2]}));
    node.local_overrides.insert(
        "CFrame".into(),
        json!({"$cframe":{"position":[10,0,0],"rotation":[1,0,0,0,1,0,0,0,1]}}),
    );
    node.local_overrides
        .insert("Size".into(), json!({"$vector3":[4,2,2]}));
    let world: WorldRef = Rc::new(RefCell::new(world));
    let points = super::mesh_world_points(&world, "mesh", &[json!([1, 0, 0])]).unwrap();
    assert_eq!(points, vec![[12.0, 0.0, 0.0]]);
}
