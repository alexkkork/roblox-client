use super::world::{
    World, WorldRef, children, emit, get_property, instance, instance_id, register_method,
    set_property,
};
use mlua::{Lua, MultiValue, Value};
use serde_json::{Value as Json, json};
use std::{cell::RefCell, rc::Rc};

fn fixture() -> WorldRef {
    let snapshot = json!({"version":1,"sequence":3,"services":{"Workspace":"ws"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{},"server_owned":true},
        {"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game","properties":{"Gravity":196.2},"server_owned":true},
        {"id":"p","class_name":"Part","name":"Part","parent":"ws","properties":{"Anchored":false},"server_owned":true}
    ]});
    Rc::new(RefCell::new(World::from_snapshot(&snapshot, None).unwrap()))
}

#[test]
fn local_constructors_and_service_bootstrap_preserve_existing_snapshot_ids() {
    let snapshot = json!({"version":1,"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{}},
        {"id":"local:1","class_name":"Folder","name":"KeepMe","parent":"game",
         "server_owned":false,"properties":{"Archivable":true}}
    ]});
    let world = Rc::new(RefCell::new(World::from_snapshot(&snapshot, None).unwrap()));
    let lua = Lua::new();
    super::world::install(&lua, &world).unwrap();
    super::services::install(&lua, &world).unwrap();
    lua.load(
        r#"
        local original = game:FindFirstChild('KeepMe')
        local created = Instance.new('Folder', game)
        local cloned = original:Clone()
        local settings = UserSettings()
        assert(original.Name == 'KeepMe' and original.Parent == game)
        assert(original ~= created and original ~= cloned)
        assert(settings:IsA('UserSettings'))
    "#,
    )
    .exec()
    .unwrap();
    assert_eq!(world.borrow().nodes["local:1"].class_name, "Folder");
    assert_eq!(world.borrow().nodes["local:1"].name, "KeepMe");
}

#[test]
fn registered_service_roots_are_parented_to_game_on_snapshot_and_delta() {
    let snapshot = json!({"version":1,"sequence":3,"services":{"Workspace":"ws"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{}},
        {"id":"ws","class_name":"Workspace","name":"Workspace","parent":null,"properties":{}},
        {"id":"ws2","class_name":"Workspace","name":"Workspace2","parent":null,"properties":{}}
    ]});
    let mut world = World::from_snapshot(&snapshot, None).unwrap();
    assert_eq!(world.nodes["ws"].parent.as_deref(), Some("game"));

    world
        .apply_delta(
            &json!({"version":1,"delta":true,"sequence":4,"actions":[],"services":{"Workspace":"ws2"}}),
            None,
        )
        .unwrap();
    assert_eq!(world.services["Workspace"], "ws2");
    assert_eq!(world.nodes["ws2"].parent.as_deref(), Some("game"));
}

#[test]
fn reads_received_property_and_reports_missing_without_fabricating_values() {
    let world = fixture();
    assert_eq!(get_property(&world, "p", "Anchored").unwrap(), json!(false));
    assert!(
        get_property(&world, "p", "Color")
            .unwrap_err()
            .to_string()
            .contains("unavailable in snapshot")
    );
}

#[test]
fn writes_are_local_overrides_with_expected_mutation_shape() {
    let world = fixture();
    let lua = Lua::new();
    super::signals::install(&lua, &world).unwrap();
    set_property(&lua, &world, "p", "Anchored", json!(true)).unwrap();
    assert_eq!(get_property(&world, "p", "Anchored").unwrap(), json!(true));
    assert_eq!(
        world.borrow().effects["mutations"][0],
        json!({"kind":"set","id":"p","property":"Anchored","value":true})
    );
    assert!(world.borrow().nodes["p"].server_owned);
}

#[test]
fn instance_handles_are_identity_cached_and_hierarchy_is_indexed() {
    let lua = Lua::new();
    let world = fixture();
    let first = instance(&lua, &world, "p").unwrap();
    let second = instance(&lua, &world, "p").unwrap();
    assert_eq!(instance_id(&first), Some("p".into()));
    assert_eq!(instance_id(&second), Some("p".into()));
    let Value::UserData(a) = first else {
        panic!("expected instance userdata")
    };
    let Value::UserData(b) = second else {
        panic!("expected instance userdata")
    };
    assert!(a.to_pointer() == b.to_pointer());
    assert_eq!(children(&world, "ws"), vec!["p"]);
}

#[test]
fn attribute_knowledge_and_deltas_preserve_server_record_ownership() {
    let snapshot = json!({"version":1,"sequence":4,"nodes":[
        {"id":"x","class_name":"Folder","name":"x","parent":null,"attributes_known":true,"attributes":{"tag":"ok"},"properties":{},"server_owned":true}
    ]});
    let mut world = World::from_snapshot(&snapshot, None).unwrap();
    world
        .apply_delta(
            &json!({"version":1,"delta":true,"sequence":5,"actions":[
                {"kind":"set","sequence":5,"id":"x","property":"Name","value":"renamed"}
            ]}),
            None,
        )
        .unwrap();
    assert_eq!(world.nodes["x"].name, "renamed");
    assert!(world.nodes["x"].server_owned);
    assert_eq!(world.nodes["x"].attributes["tag"], json!("ok"));
}

#[test]
fn effect_emitter_appends_without_replacing_prior_entries() {
    let world = fixture();
    emit(&world, "events", json!({"kind":"one"}));
    emit(&world, "events", json!({"kind":"two"}));
    assert_eq!(
        world.borrow().effects["events"].as_array().unwrap().len(),
        2
    );
}

#[test]
fn registered_methods_strip_only_the_receiver() {
    fn echo(_lua: &Lua, _ctx: &WorldRef, _id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
        Ok(args)
    }

    let lua = Lua::new();
    let world = fixture();
    register_method(&world, "Part", "Echo", echo);
    super::world::install(&lua, &world).unwrap();
    lua.globals()
        .set("part", instance(&lua, &world, "p").unwrap())
        .unwrap();

    let returned: Value = lua.load("return part:Echo(part)").eval().unwrap();
    assert!(matches!(returned, Value::UserData(_)));

    let error = lua
        .load("return part.Echo('payload')")
        .eval::<Value>()
        .unwrap_err();
    assert!(error.to_string().contains("receiver"));
}

#[test]
fn actor_world_sync_preserves_actor_local_nodes_and_overrides() {
    let root = fixture();
    let actor = Rc::new(RefCell::new(root.borrow().fork_for_actor("abc")));
    let lua = Lua::new();
    super::signals::install(&lua, &actor).unwrap();
    super::world::install(&lua, &actor).unwrap();
    let local = super::world::create_instance(&lua, &actor, "Folder", Some("ws")).unwrap();
    let actor_id = instance_id(&local).unwrap();
    assert!(actor_id.starts_with("local:actor:616263:"));
    actor
        .borrow_mut()
        .nodes
        .get_mut("p")
        .unwrap()
        .local_overrides
        .insert("Anchored".into(), json!(true));

    {
        let mut root = root.borrow_mut();
        root.nodes
            .get_mut("p")
            .unwrap()
            .revisions
            .insert("Anchored".into(), json!(8));
        root.sequence = 8;
    }
    actor.borrow_mut().sync_actor_fork(&root.borrow());

    assert!(actor.borrow().nodes.contains_key(&actor_id));
    assert_eq!(
        actor.borrow().nodes["p"].local_overrides["Anchored"],
        json!(true)
    );
    assert_eq!(actor.borrow().nodes["p"].revisions["Anchored"], json!(8));
    assert_eq!(actor.borrow().sequence, 8);
    assert_eq!(actor.borrow().nodes[&actor_id].class_name, "Folder");
    assert_eq!(actor.borrow().nodes[&actor_id].name, "Folder");
    assert_eq!(
        actor.borrow().nodes[&actor_id].parent.as_deref(),
        Some("ws")
    );
    assert!(super::world::ordered_ids(&actor).contains(&actor_id));
}

#[test]
fn instance_query_descendants_matches_snapshot_classes_names_properties_and_relations() {
    let snapshot = json!({"version":1,"sequence":1,"services":{"Workspace":"ws"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{}},
        {"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game","properties":{}},
        {"id":"actor","class_name":"Actor","name":"Actor","parent":"ws","properties":{}},
        {"id":"part","class_name":"Part","name":"Probe","parent":"actor","properties":{"Anchored":false},"collection_tags":["probe"]},
        {"id":"other","class_name":"Part","name":"Other","parent":"ws","properties":{"Anchored":true}}
    ]});
    let world = Rc::new(RefCell::new(World::from_snapshot(&snapshot, None).unwrap()));
    let lua = Lua::new();
    super::world::install(&lua, &world).unwrap();
    let result: bool = lua
        .load(
            r#"
        local direct = workspace:QueryDescendants("> Actor > Part#Probe[Anchored=false]")
        assert(#direct == 1 and direct[1].Name == "Probe")
        local has = workspace:QueryDescendants("Actor:has(Part)")
        assert(#has == 1 and has[1].ClassName == "Actor")
        local not_other = workspace:QueryDescendants("Part:not(#Other)")
        assert(#not_other == 1 and not_other[1].Name == "Probe")
        return true
    "#,
        )
        .eval()
        .unwrap();
    assert!(result);
}

#[test]
fn property_default_methods_reset_only_known_writable_values_and_get_actor_walks_ancestors() {
    let snapshot = json!({"version":1,"sequence":1,"services":{"Workspace":"ws"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{}},
        {"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game","properties":{}},
        {"id":"actor","class_name":"Actor","name":"Actor","parent":"ws","properties":{}},
        {"id":"part","class_name":"Part","name":"Part","parent":"actor","properties":{"Anchored":false}}
    ]});
    let world = Rc::new(RefCell::new(World::from_snapshot(&snapshot, None).unwrap()));
    let lua = Lua::new();
    super::signals::install(&lua, &world).unwrap();
    super::world::install(&lua, &world).unwrap();
    let result: bool = lua
        .load(
            r#"
        local p=workspace:FindFirstChild("Actor"):FindFirstChild("Part")
        assert(p:GetActor().Name=="Actor")
        assert(not p:IsPropertyModified("Anchored"))
        p.Anchored=true
        assert(p:IsPropertyModified("Anchored"))
        p:ResetPropertyToDefault("Anchored")
        assert(p.Anchored==false and not p:IsPropertyModified("Anchored"))
        p.Name="Changed"
        assert(p:IsPropertyModified("Name"))
        p:ResetPropertyToDefault("Name")
        assert(p.Name=="Part" and not p:IsPropertyModified("Name"))
        return true
    "#,
        )
        .eval()
        .unwrap();
    assert!(result);
}

#[test]
fn client_property_origin_reports_received_local_and_unknown_snapshot_state() {
    let world = fixture();
    let lua = Lua::new();
    super::signals::install(&lua, &world).unwrap();
    super::world::install(&lua, &world).unwrap();
    lua.globals()
        .set("part", instance(&lua, &world, "p").unwrap())
        .unwrap();
    let origins: (String, String, String) = lua
        .load(
            r#"
        local received=client.propertyOrigin(part,"Anchored")
        part.Anchored=true
        return received,client.propertyOrigin(part,"Anchored"),client.propertyOrigin(part,"Color")
    "#,
        )
        .eval()
        .unwrap();
    assert_eq!(
        origins,
        ("received".into(), "local".into(), "unavailable".into())
    );
}

#[test]
fn engine_global_singletons_are_stable_local_nodes_without_create_mutations() {
    let world = fixture();
    let lua = Lua::new();
    super::world::install(&lua, &world).unwrap();
    let mutation_count = world
        .borrow()
        .effects
        .get("mutations")
        .and_then(Json::as_array)
        .map_or(0, Vec::len);
    let settings = super::world::ensure_engine_global_node(&lua, &world, "GlobalSettings").unwrap();
    let settings_id = instance_id(&settings).unwrap();
    let again = super::world::ensure_engine_global_node(&lua, &world, "GlobalSettings").unwrap();
    assert_eq!(instance_id(&again).as_deref(), Some(settings_id.as_str()));
    let plugin = super::world::ensure_engine_global_node(&lua, &world, "PluginManager").unwrap();
    lua.globals().set("settings", settings).unwrap();
    lua.globals().set("plugin", plugin).unwrap();
    let ancestry: bool = lua.load("return settings:IsA('GenericSettings') and settings:IsA('ServiceProvider') and plugin:IsA('Instance')").eval().unwrap();
    assert!(ancestry);
    assert_eq!(
        world
            .borrow()
            .effects
            .get("mutations")
            .and_then(Json::as_array)
            .map_or(0, Vec::len),
        mutation_count
    );
    assert!(!world.borrow().services.contains_key("PluginManager"));

    world
        .borrow_mut()
        .nodes
        .get_mut(&settings_id)
        .unwrap()
        .destroyed = true;
    let recreated =
        super::world::ensure_engine_global_node(&lua, &world, "GlobalSettings").unwrap();
    assert_eq!(
        instance_id(&recreated).as_deref(),
        Some(settings_id.as_str())
    );
    assert!(!world.borrow().nodes[&settings_id].destroyed);
    assert_eq!(
        world
            .borrow()
            .effects
            .get("mutations")
            .and_then(Json::as_array)
            .map_or(0, Vec::len),
        mutation_count
    );
}

#[test]
fn instance_handle_constructor_null_get_identity_and_lazy_service_lookup() {
    let world = fixture();
    let lua = Lua::new();
    super::world::install(&lua, &world).unwrap();
    lua.globals()
        .set("part", instance(&lua, &world, "p").unwrap())
        .unwrap();
    let encoded_null = super::values::to_json(
        &lua,
        &super::world::value_from_json(&lua, &world, &json!({"$instance_handle":null})).unwrap(),
    )
    .unwrap();
    assert_eq!(encoded_null, json!({"$instance_handle":null}));
    let result: bool = lua
        .load(
            r#"
        local first=InstanceHandle.new(part)
        local second=InstanceHandle.new(part)
        assert(first==second and first:Get()==part)
        local null=InstanceHandle.new()
        assert(null:Get()==nil and tostring(null)=="nil")
        assert(null==InstanceHandle.new(nil))
        assert(first.Get(null)==nil and null.Get(first)==part)
        assert(first.Wait(null,0.01)==nil and null.Wait(first,0.01)==part)
        assert(not pcall(function() first:Wait(0) end))
        assert(not pcall(function() null:Wait(-1) end))
        assert(game:GetService("UserInputService")==game:FindService("UserInputService"))
        assert(game:FindService("PathfindingService")==nil)
        local path=game:GetService("PathfindingService")
        assert(path==game:GetService("PathfindingService"))
        assert(not pcall(function() workspace:GetService("RunService") end))
        assert(not pcall(function() game:GetService("NotARealService") end))
        return true
    "#,
        )
        .eval()
        .unwrap();
    assert!(result);
}

#[test]
fn instance_handle_wait_resolves_after_received_node_delta() {
    use std::time::{Duration, Instant};
    let world = fixture();
    let lua = Lua::new();
    let scheduler = Rc::new(RefCell::new(super::scheduler::Scheduler::default()));
    super::scheduler::install(&lua, &scheduler).unwrap();
    super::world::install(&lua, &world).unwrap();
    let handle =
        super::world::value_from_json(&lua, &world, &json!({"$instance_handle":"future-node"}))
            .unwrap();
    let function: mlua::Function = lua
        .load("return function(handle) local node=handle:Wait(); return node and node.Name end")
        .eval()
        .unwrap();
    let thread = super::scheduler::enqueue_with_policy(
        &lua,
        function,
        mlua::MultiValue::from_vec(vec![handle]),
        super::scheduler::TaskPolicy::default(),
    )
    .unwrap();
    let report = super::scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_secs(1));
    assert!(
        report.errors.is_empty(),
        "initial Wait step errors: {:?}",
        report.errors
    );
    assert!(super::scheduler::is_parked(&scheduler, &thread));

    world.borrow_mut().apply_delta(&json!({"version":1,"delta":true,"sequence":4,"actions":[{
        "sequence":4,"kind":"create","node":{"id":"future-node","class_name":"Folder","name":"Arrived","parent":"ws","properties":{}}
    }]}), None).unwrap();
    super::world::resolve_instance_handle_waiters(&lua, &world).unwrap();
    let report = super::scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_secs(1));
    assert!(
        report.errors.is_empty(),
        "resolved Wait step errors: {:?}",
        report.errors
    );
    match super::scheduler::take_outcome(&scheduler, &thread).unwrap() {
        super::scheduler::TaskOutcome::Completed(values) => assert_eq!(
            values.front(),
            Some(&Value::String(lua.create_string("Arrived").unwrap()))
        ),
        other => panic!("expected resolved handle result, got {other:?}"),
    }
}

#[test]
fn instance_handle_wait_is_scoped_to_its_world() {
    use std::time::{Duration, Instant};
    let first_world = fixture();
    let second_world = fixture();
    let lua = Lua::new();
    let scheduler = Rc::new(RefCell::new(super::scheduler::Scheduler::default()));
    super::scheduler::install(&lua, &scheduler).unwrap();
    super::world::install(&lua, &first_world).unwrap();
    let handle =
        super::world::value_from_json(&lua, &first_world, &json!({"$instance_handle":"same-id"}))
            .unwrap();
    let function: mlua::Function = lua
        .load("return function(handle) return handle:Wait() end")
        .eval()
        .unwrap();
    let thread = super::scheduler::enqueue_with_policy(
        &lua,
        function,
        MultiValue::from_vec(vec![handle]),
        super::scheduler::TaskPolicy::default(),
    )
    .unwrap();
    let report = super::scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_secs(1));
    assert!(
        report.errors.is_empty(),
        "initial Wait step errors: {:?}",
        report.errors
    );
    assert!(super::scheduler::is_parked(&scheduler, &thread));

    second_world.borrow_mut().apply_delta(&json!({"version":1,"delta":true,"sequence":4,"actions":[{
        "sequence":4,"kind":"create","node":{"id":"same-id","class_name":"Folder","name":"Wrong world","parent":"ws","properties":{}}
    }]}), None).unwrap();
    super::world::resolve_instance_handle_waiters(&lua, &second_world).unwrap();
    assert!(super::scheduler::is_parked(&scheduler, &thread));

    first_world.borrow_mut().apply_delta(&json!({"version":1,"delta":true,"sequence":4,"actions":[{
        "sequence":4,"kind":"create","node":{"id":"same-id","class_name":"Folder","name":"Right world","parent":"ws","properties":{}}
    }]}), None).unwrap();
    super::world::resolve_instance_handle_waiters(&lua, &first_world).unwrap();
    let report = super::scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_secs(1));
    assert!(
        report.errors.is_empty(),
        "resolved Wait step errors: {:?}",
        report.errors
    );
    match super::scheduler::take_outcome(&scheduler, &thread).unwrap() {
        super::scheduler::TaskOutcome::Completed(values) => {
            let Value::UserData(node) = values.front().unwrap() else {
                panic!("expected resolved instance")
            };
            assert_eq!(
                instance_id(&Value::UserData(node.clone())).as_deref(),
                Some("same-id")
            );
            assert_eq!(first_world.borrow().nodes["same-id"].name, "Right world");
        }
        other => panic!("expected resolved handle result, got {other:?}"),
    }
}

#[test]
fn user_settings_service_provider_returns_user_game_settings() {
    let world = fixture();
    let lua = Lua::new();
    super::world::install(&lua, &world).unwrap();
    super::services::install(&lua, &world).unwrap();
    let result: bool = lua
        .load(
            r#"
        local settings=UserSettings()
        local first=settings:GetService("UserGameSettings")
        local second=settings:FindService("UserGameSettings")
        assert(first==second and first.ClassName=="UserGameSettings")
        assert(settings:FindService("MissingSettingsService")==nil)
        assert(not pcall(function() settings:GetService("MissingSettingsService") end))
        return true
    "#,
        )
        .eval()
        .unwrap();
    assert!(result);
}

#[test]
fn local_part_mass_defaults_are_typed_modifiable_and_resettable() {
    let world = fixture();
    let lua = Lua::new();
    super::enums::install(&lua).unwrap();
    super::signals::install(&lua, &world).unwrap();
    super::world::install(&lua, &world).unwrap();
    let result: bool = lua
        .load(
            r#"
        local part=Instance.new("Part")
        assert(typeof(part.Shape)=="EnumItem" and part.Shape==Enum.PartType.Block)
        assert(typeof(part.Material)=="EnumItem" and part.Material==Enum.Material.Plastic)
        assert(not part:IsPropertyModified("Shape"))
        assert(not part:IsPropertyModified("Material"))

        part.Shape=Enum.PartType.Ball
        part.Material=Enum.Material.Metal
        assert(part:IsPropertyModified("Shape"))
        assert(part:IsPropertyModified("Material"))
        part:ResetPropertyToDefault("Shape")
        part:ResetPropertyToDefault("Material")
        assert(part.Shape==Enum.PartType.Block and not part:IsPropertyModified("Shape"))
        assert(part.Material==Enum.Material.Plastic and not part:IsPropertyModified("Material"))
        return true
    "#,
        )
        .eval()
        .unwrap();
    assert!(result);

    // This is received state, so an omitted wire value stays unavailable.
    assert!(
        get_property(&world, "p", "Shape")
            .unwrap_err()
            .to_string()
            .contains("unavailable in snapshot")
    );
    assert!(
        get_property(&world, "p", "Material")
            .unwrap_err()
            .to_string()
            .contains("unavailable in snapshot")
    );
}
