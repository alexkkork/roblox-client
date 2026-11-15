use super::super::scheduler;
use super::*;
use mlua::AnyUserData;
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

#[path = "attachment_actor_tests.rs"]
mod attachment_tests;
#[path = "model_scaling_actor_tests.rs"]
mod model_scaling_tests;

fn setup_worlds() -> (Lua, WorldRef, Lua, WorldRef) {
    let snapshot = json!({
        "version":1,"sequence":0,
        "services":{"Workspace":"workspace","CollectionService":"collection"},
        "nodes":[
            {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{}},
            {"id":"workspace","class_name":"Workspace","name":"Workspace","parent":"game","properties":{}},
            {"id":"collection","class_name":"CollectionService","name":"CollectionService","parent":"game","properties":{}},
            {"id":"worker","class_name":"Actor","name":"worker","parent":"workspace","properties":{}},
            {"id":"part","class_name":"Part","name":"Part","parent":"workspace","properties":{"Anchored":false},"attributes":{},"attributes_known":true,"collection_tags":[],"collection_tags_known":true}
        ]
    });
    let root_world = Rc::new(RefCell::new(
        world::World::from_snapshot(&snapshot, None).unwrap(),
    ));
    let root_lua = Lua::new();
    let root_scheduler = Rc::new(RefCell::new(scheduler::Scheduler::new()));
    scheduler::install(&root_lua, &root_scheduler).unwrap();
    super::super::install_bindings(&root_lua, &root_world).unwrap();

    let actor_world = Rc::new(RefCell::new(root_world.borrow().fork_for_actor("worker")));
    let actor_lua = Lua::new();
    let actor_scheduler = Rc::new(RefCell::new(scheduler::Scheduler::new()));
    scheduler::install(&actor_lua, &actor_scheduler).unwrap();
    super::super::install_bindings(&actor_lua, &actor_world).unwrap();
    (root_lua, root_world, actor_lua, actor_world)
}

fn actor_journal(actor_world: &WorldRef) -> Vec<Json> {
    actor_world
        .borrow()
        .effects
        .get("mutations")
        .and_then(Json::as_array)
        .cloned()
        .unwrap_or_default()
}

fn actor_instance(lua: &Lua, world: &WorldRef, id: &str) -> AnyUserData {
    match world::instance(lua, world, id).unwrap() {
        Value::UserData(value) => value,
        _ => panic!("expected Instance userdata"),
    }
}

fn prepare_pivot_model(root_lua: &Lua, root: &WorldRef, actor: &WorldRef) {
    root_lua
        .load(
            r#"
        local m=Instance.new("Model",workspace);m.Name="Mover"
        local a=Instance.new("Part",m);a.Name="A";a.CFrame=CFrame.new(-2,0,0)
        local b=Instance.new("Part",m);b.Name="B";b.CFrame=CFrame.new(2,0,0)
        m.WorldPivot=CFrame.new()
    "#,
        )
        .exec()
        .unwrap();
    actor.borrow_mut().sync_actor_fork(&root.borrow());
    root.borrow_mut().effects.clear();
    actor.borrow_mut().effects.clear();
}

#[test]
fn actor_model_pivots_merge_as_atomic_batches_with_matching_revision_fences() {
    let (root_lua, root, actor_lua, actor) = setup_worlds();
    prepare_pivot_model(&root_lua, &root, &actor);
    actor_lua
        .load(
            r#"
        workspace.Mover:PivotTo(CFrame.new(10,0,0))
        workspace.Mover:PivotTo(CFrame.new(20,0,0))
    "#,
        )
        .exec()
        .unwrap();
    let journal = actor_journal(&actor);
    assert_eq!(journal.len(), 2);
    assert!(journal.iter().all(|row| row["kind"] == "pose_batch"));
    let report = merge_actor_journal(&root_lua, &root, &actor, "worker", &journal).unwrap();
    assert!(report.conflicts.is_empty(), "{:?}", report.conflicts);
    assert_eq!(report.applied, 2);
    root_lua
        .load(
            r#"
        local m=workspace.Mover
        assert(m.A.Position.X==18 and m.B.Position.X==22 and m:GetPivot().X==20)
    "#,
        )
        .exec()
        .unwrap();
    assert!(root.borrow().effects["mutations"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row["kind"] == "set"
            && row["actor_origin"] == "worker"
            && row["_pivot_cframe_only"] == true));
}

#[test]
fn stale_actor_pivot_rejects_the_entire_batch_without_moving_other_parts() {
    let (root_lua, root, actor_lua, actor) = setup_worlds();
    prepare_pivot_model(&root_lua, &root, &actor);
    actor_lua
        .load("workspace.Mover:PivotTo(CFrame.new(10,0,0))")
        .exec()
        .unwrap();
    root_lua
        .load("workspace.Mover.A.CFrame=CFrame.new(99,0,0)")
        .exec()
        .unwrap();
    let journal = actor_journal(&actor);
    let report = merge_actor_journal(&root_lua, &root, &actor, "worker", &journal).unwrap();
    assert_eq!(report.applied, 0);
    assert_eq!(report.conflicts.len(), 1);
    root_lua
        .load(
            r#"
        local m=workspace.Mover
        assert(m.A.Position.X==99 and m.B.Position.X==2 and m:GetPivot().X==0)
    "#,
        )
        .exec()
        .unwrap();
}

#[test]
fn actor_properties_attributes_and_tags_merge_through_root_setters() {
    let (root_lua, root_world, actor_lua, actor_world) = setup_worlds();
    let part = actor_instance(&actor_lua, &actor_world, "part");
    part.set("Anchored", true).unwrap();
    part.call_method::<()>("SetAttribute", ("ActorMark", 73))
        .unwrap();
    let collection = actor_instance(&actor_lua, &actor_world, "collection");
    let target = actor_instance(&actor_lua, &actor_world, "part");
    collection
        .call_method::<()>("AddTag", (target, "actor-tag"))
        .unwrap();
    let journal = actor_journal(&actor_world);

    let report =
        merge_actor_journal(&root_lua, &root_world, &actor_world, "worker", &journal).unwrap();
    assert_eq!(report.applied, 3);
    assert!(report.conflicts.is_empty());
    assert_eq!(
        world::get_property(&root_world, "part", "Anchored").unwrap(),
        json!(true)
    );
    let root_part = actor_instance(&root_lua, &root_world, "part");
    assert_eq!(
        root_part
            .call_method::<i64>("GetAttribute", "ActorMark")
            .unwrap(),
        73
    );
    assert_eq!(
        root_world.borrow().nodes["part"].collection_tags,
        ["actor-tag"]
    );
    assert!(
        root_world.borrow().effects["mutations"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|row| row["actor_origin"] == "worker")
            .count()
            >= 3
    );
}

#[test]
fn actor_create_is_adopted_only_with_its_reserved_instance_id() {
    let (root_lua, root_world, actor_lua, actor_world) = setup_worlds();
    let created =
        world::create_instance(&actor_lua, &actor_world, "Folder", Some("workspace")).unwrap();
    let id = world::instance_id(&created).unwrap();
    assert!(id.starts_with("local:actor:776f726b6572:"));
    let journal = actor_journal(&actor_world);
    let report =
        merge_actor_journal(&root_lua, &root_world, &actor_world, "worker", &journal).unwrap();
    assert_eq!(report.applied, 2);
    assert!(root_world.borrow().nodes.contains_key(&id));
    assert_eq!(
        root_world.borrow().nodes[&id].parent.as_deref(),
        Some("workspace")
    );

    let fabricated = json!({"kind":"create","node":{
        "id":"local:actor:6f74686572:1","class_name":"Folder","name":"fake",
        "parent":"workspace","properties":{},"server_owned":false
    }});
    assert!(merge_actor_journal(
        &root_lua,
        &root_world,
        &actor_world,
        "worker",
        &[fabricated]
    )
    .is_err());
}

#[test]
fn stale_actor_property_write_is_rejected_and_local_override_is_cleared() {
    let (root_lua, root_world, actor_lua, actor_world) = setup_worlds();
    let part = actor_instance(&actor_lua, &actor_world, "part");
    part.set("Anchored", true).unwrap();
    let journal = actor_journal(&actor_world);
    world::set_property(&root_lua, &root_world, "part", "Anchored", json!(false)).unwrap();
    let report =
        merge_actor_journal(&root_lua, &root_world, &actor_world, "worker", &journal).unwrap();
    assert_eq!(report.applied, 0);
    assert_eq!(report.conflicts, ["set:part.Anchored"]);
    assert_eq!(
        world::get_property(&root_world, "part", "Anchored").unwrap(),
        json!(false)
    );
    assert_eq!(
        world::get_property(&actor_world, "part", "Anchored").unwrap(),
        json!(false)
    );
}

#[test]
fn committed_actor_property_change_fires_in_peer_vm() {
    let (root_lua, root_world, actor_lua, actor_world) = setup_worlds();
    let peer_world = Rc::new(RefCell::new(root_world.borrow().fork_for_actor("peer")));
    let peer_lua = Lua::new();
    let peer_scheduler = Rc::new(RefCell::new(scheduler::Scheduler::new()));
    scheduler::install(&peer_lua, &peer_scheduler).unwrap();
    super::super::install_bindings(&peer_lua, &peer_world).unwrap();
    let peer_part = world::instance(&peer_lua, &peer_world, "part").unwrap();
    peer_lua.globals().set("peer_part", peer_part).unwrap();
    peer_lua
        .load("seen=false; peer_part:GetPropertyChangedSignal('Anchored'):Connect(function() seen=true end)")
        .exec()
        .unwrap();

    let part = actor_instance(&actor_lua, &actor_world, "part");
    part.set("Anchored", true).unwrap();
    let journal = actor_journal(&actor_world);
    merge_actor_journal(&root_lua, &root_world, &actor_world, "worker", &journal).unwrap();
    sync_peer(&peer_lua, &peer_world, &root_world, &journal).unwrap();
    scheduler::step(
        &peer_lua,
        &peer_scheduler,
        Instant::now(),
        Duration::from_millis(30),
    );
    assert!(peer_lua.globals().get::<bool>("seen").unwrap());
}

#[test]
fn status_strips_internal_fences_but_keeps_actor_origin() {
    let mut status = json!({"actor_id":"worker","mutations":[{
        "kind":"set","_base_local_revision":4
    }],"world_effects":{"mutations":[{
        "kind":"set","_base_local_revision":4
    }]}});
    strip_internal_fields(&mut status);
    assert_eq!(status["mutations"][0]["actor_origin"], "worker");
    assert_eq!(
        status["world_effects"]["mutations"][0]["actor_origin"],
        "worker"
    );
    assert!(status["mutations"][0].get("_base_local_revision").is_none());
    assert!(status["world_effects"]["mutations"][0]
        .get("_base_local_revision")
        .is_none());
}
