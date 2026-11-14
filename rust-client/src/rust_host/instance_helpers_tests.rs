use super::{
    bindables, instance_helpers, remotes, signals,
    world::{self, World, WorldRef},
};
use mlua::Lua;
use serde_json::json;
use std::{cell::RefCell, rc::Rc};

fn setup() -> (Lua, WorldRef) {
    let snapshot = json!({"version":1,"sequence":1,"services":{"Players":"players"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{},"server_owned":true},
        {"id":"players","class_name":"Players","name":"Players","parent":"game","properties":{"LocalPlayer":{"$ref":"player"}},"server_owned":true},
        {"id":"player","class_name":"Player","name":"Tester","parent":"players","properties":{"UserId":12,"SimulationRadius":128},"server_owned":true},
        {"id":"part","class_name":"Part","name":"Part","parent":"game","properties":{"Anchored":false,"NetworkOwnerV3":7},"server_owned":true},
        {"id":"bindable","class_name":"BindableFunction","name":"BindableFunction","parent":"game","properties":{},"server_owned":false}
    ]});
    let world = Rc::new(RefCell::new(World::from_snapshot(&snapshot, None).unwrap()));
    let lua = Lua::new();
    signals::install(&lua, &world).unwrap();
    world::install(&lua, &world).unwrap();
    bindables::install(&lua, &world).unwrap();
    remotes::install(&lua, &world).unwrap();
    instance_helpers::install(&lua, &world).unwrap();
    (lua, world)
}

#[test]
fn callback_helpers_use_the_bindable_callback_the_invoke_path_uses() {
    let (lua, _world) = setup();
    let same: bool = lua
        .load(
            r#"
        local object = game:FindFirstChild("BindableFunction")
        local callback = function(value) return value + 1 end
        setcallbackfunction(object, "OnInvoke", callback)
        return getcallbackfunction(object, "OnInvoke") == callback
            and getcallbackvalue(object, "OnInvoke") == callback
    "#,
        )
        .eval()
        .unwrap();
    assert!(same);
}

#[test]
fn callback_thread_and_instance_cache_are_local() {
    let (lua, _world) = setup();
    let same: bool = lua
        .load(
            r#"
        local object = game:FindFirstChild("BindableFunction")
        local thread = coroutine.create(function() end)
        setcallbackthread(object, "OnInvoke", thread)
        return getcallbackthread(object, "OnInvoke") == thread
    "#,
        )
        .eval()
        .unwrap();
    assert!(same);
    let same_ref_and_listed: bool = lua
        .load(
            r#"
        local part = game:FindFirstChild("Part")
        local bindable = game:FindFirstChild("BindableFunction")
        local found = {}
        for _, item in ipairs(getinstancecache()) do
            assert(compareinstances(item, cloneref(item)))
            found[item.Name] = true
        end
        return clonereference(part) == part
            and found.Part == true
            and found.BindableFunction == true
            and found.game == true
    "#,
        )
        .eval()
        .unwrap();
    assert!(same_ref_and_listed);
}

#[test]
fn local_radius_and_owner_queries_require_received_values() {
    let (lua, _world) = setup();
    let radius: f64 = lua.load("return getsimulationradius()").eval().unwrap();
    let owner: bool = lua
        .load(r#"return getnetworkowner(game:FindFirstChild("Part"))"#)
        .eval()
        .unwrap();
    assert_eq!(radius, 128.0);
    assert!(owner);
}
