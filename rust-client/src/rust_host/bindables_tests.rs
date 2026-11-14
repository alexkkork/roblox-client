use super::{
    bindables, errors,
    scheduler::{self, Scheduler},
    signals,
    world::{self, World, WorldRef},
};
use mlua::{Lua, Value};
use serde_json::json;
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

fn fixture() -> (Lua, WorldRef, scheduler::SchedulerRef) {
    let lua = Lua::new();
    let snapshot = json!({"version":1,"sequence":0,"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{},"server_owned":true}
    ]});
    let world = Rc::new(RefCell::new(World::from_snapshot(&snapshot, None).unwrap()));
    let sched = Rc::new(RefCell::new(Scheduler::new()));
    scheduler::install(&lua, &sched).unwrap();
    errors::install(&lua).unwrap();
    signals::install(&lua, &world).unwrap();
    world::install(&lua, &world).unwrap();
    bindables::install(&lua, &world).unwrap();
    (lua, world, sched)
}

fn step(lua: &Lua, sched: &scheduler::SchedulerRef, now: Instant) -> scheduler::StepReport {
    scheduler::step(lua, sched, now, Duration::from_millis(100))
}

#[test]
fn bindable_event_fire_schedules_local_callbacks_in_order_with_nil_values() {
    let (lua, _world, sched) = fixture();
    lua.load(r#"
        event = Instance.new("BindableEvent")
        called = {}
        event.Event:Connect(function(a,b,c) called[1]="first"; called.a=a; called.b=b; called.c=c end)
        event.Event:Connect(function() called[2]="second" end)
        event:Fire(event, nil, 3)
        assert(called[1] == nil)
    "#).exec().unwrap();
    let report = step(&lua, &sched, Instant::now());
    assert_eq!(report.resumed, 2);
    assert_eq!(
        lua.load("return called[1], called[2], called.a == event, called.b, called.c")
            .eval::<(String, String, bool, Value, i64)>()
            .unwrap(),
        ("first".into(), "second".into(), true, Value::Nil, 3)
    );
}

#[test]
fn bindable_function_returns_exact_multi_value_tuple() {
    let (lua, _world, sched) = fixture();
    lua.load(
        r#"
        fn = Instance.new("BindableFunction")
        fn.OnInvoke = function(a,b) return a+1, nil, b.."!" end
        result = nil
        task.spawn(function()
            local a,b,c = fn:Invoke(41, "ok")
            result = {a=a,b=b,c=c}
        end)
    "#,
    )
    .exec()
    .unwrap();
    step(&lua, &sched, Instant::now());
    assert_eq!(
        lua.load("return result.a, result.b, result.c")
            .eval::<(i64, Value, String)>()
            .unwrap(),
        (42, Value::Nil, "ok!".into())
    );
}

#[test]
fn bindable_function_waits_for_yielding_callback_before_returning_values() {
    let (lua, _world, sched) = fixture();
    lua.load(
        r#"
        fn = Instance.new("BindableFunction")
        fn.OnInvoke = function(value) task.wait(0.01); return value, nil, "after" end
        result = nil
        task.spawn(function() result = table.pack(fn:Invoke("before")) end)
    "#,
    )
    .exec()
    .unwrap();
    step(&lua, &sched, Instant::now());
    assert!(lua.load("return result == nil").eval::<bool>().unwrap());
    step(&lua, &sched, Instant::now() + Duration::from_millis(50));
    assert_eq!(
        lua.load("return result[1], result[2], result[3], result.n")
            .eval::<(String, Value, String, i64)>()
            .unwrap(),
        ("before".into(), Value::Nil, "after".into(), 3)
    );
}

#[test]
fn invoke_without_callback_errors_and_nil_clears_the_callback() {
    let (lua, _world, sched) = fixture();
    lua.load(
        r#"
        fn = Instance.new("BindableFunction")
        task.spawn(function()
            local ok, err = pcall(function() fn:Invoke() end)
            invoke_error = not ok and string.find(err, "no OnInvoke callback") ~= nil
        end)
    "#,
    )
    .exec()
    .unwrap();
    step(&lua, &sched, Instant::now());
    assert_eq!(lua.globals().get::<bool>("invoke_error").unwrap(), true);
    lua.load(r#"fn.OnInvoke = function() return 1 end; fn.OnInvoke = nil"#)
        .exec()
        .unwrap();
    lua.load(
        r#"
        task.spawn(function()
            local ok, err = pcall(function() fn:Invoke() end)
            cleared_error = not ok and string.find(err, "no OnInvoke callback") ~= nil
        end)
    "#,
    )
    .exec()
    .unwrap();
    step(&lua, &sched, Instant::now());
    assert_eq!(lua.globals().get::<bool>("cleared_error").unwrap(), true);
}

#[test]
fn bindable_callbacks_are_scoped_to_the_world_identity_and_local_instances() {
    let (lua, first_world, _sched) = fixture();
    let second_world = Rc::new(RefCell::new(World::from_snapshot(
        &json!({"version":1,"sequence":0,"nodes":[
            {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{},"server_owned":true},
            {"id":"local:shared-bindable","class_name":"BindableFunction","name":"BindableFunction","parent":null,"properties":{},"server_owned":false}
        ]}), None).unwrap()));
    bindables::install(&lua, &second_world).unwrap();
    let local_id = first_world
        .borrow()
        .nodes
        .get("local:shared-bindable")
        .map(|n| n.id.clone());
    assert!(local_id.is_none());
    let foreign = world::instance(&lua, &second_world, "local:shared-bindable").unwrap();
    lua.globals().set("foreign", foreign).unwrap();
    lua.load(r#"foreign.OnInvoke = function() return "wrong world" end"#)
        .exec()
        .unwrap();
    // An instance in the first world with the same identifier must not see the callback.
    let local_node = world::Node {
        id: "local:shared-bindable".into(),
        name: "BindableFunction".into(),
        class_name: "BindableFunction".into(),
        parent: None,
        destroyed: false,
        server_owned: false,
        properties: Default::default(),
        local_overrides: Default::default(),
        attributes: Default::default(),
        attributes_known: true,
        revisions: Default::default(),
        local_revision: 0,
        collection_tags: Vec::new(),
        collection_tags_known: true,
        projected: None,
    };
    first_world
        .borrow_mut()
        .nodes
        .insert("local:shared-bindable".into(), local_node);
    let local = world::instance(&lua, &first_world, "local:shared-bindable").unwrap();
    lua.globals().set("local_fn", local).unwrap();
    lua.load(
        r#"
        task.spawn(function()
            local ok, err = pcall(function() local_fn:Invoke() end)
            identity_error = not ok and string.find(err, "no OnInvoke callback") ~= nil
        end)
    "#,
    )
    .exec()
    .unwrap();
    let sched = lua
        .app_data_ref::<scheduler::SchedulerRef>()
        .unwrap()
        .clone();
    step(&lua, &sched, Instant::now());
    assert_eq!(lua.globals().get::<bool>("identity_error").unwrap(), true);
}

#[test]
fn snapshot_bindables_execute_callbacks_entirely_inside_the_luau_vm() {
    let lua = Lua::new();
    let snapshot = json!({"version":1,"sequence":0,"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{},"server_owned":true},
        {"id":"received-event","class_name":"BindableEvent","name":"ReceivedEvent","parent":"game","properties":{},"server_owned":true},
        {"id":"received-function","class_name":"BindableFunction","name":"ReceivedFunction","parent":"game","properties":{},"server_owned":true}
    ]});
    let world = Rc::new(RefCell::new(World::from_snapshot(&snapshot, None).unwrap()));
    let sched = Rc::new(RefCell::new(Scheduler::new()));
    scheduler::install(&lua, &sched).unwrap();
    errors::install(&lua).unwrap();
    signals::install(&lua, &world).unwrap();
    world::install(&lua, &world).unwrap();
    bindables::install(&lua, &world).unwrap();
    let bootstrap_mutations = world.borrow().effects.get("mutations").cloned();
    lua.globals()
        .set(
            "event",
            world::instance(&lua, &world, "received-event").unwrap(),
        )
        .unwrap();
    lua.globals()
        .set(
            "fn",
            world::instance(&lua, &world, "received-function").unwrap(),
        )
        .unwrap();
    lua.load(
        r#"
        event_calls = 0
        event.Event:Connect(function(value) event_calls = event_calls + value end)
        event:Fire(4)
        fn.OnInvoke = function(value) return value * 2, nil end
        result = nil
        task.spawn(function() result = table.pack(fn:Invoke(9)) end)
    "#,
    )
    .exec()
    .unwrap();
    step(&lua, &sched, Instant::now());
    assert_eq!(
        lua.load("return event_calls, result[1], result[2], result.n")
            .eval::<(i64, i64, Value, i64)>()
            .unwrap(),
        (4, 18, Value::Nil, 2)
    );
    assert_eq!(
        world.borrow().effects.get("mutations"),
        bootstrap_mutations.as_ref(),
        "Bindable execution must not add a world/server mutation"
    );
}

#[test]
fn destroying_a_bindable_wakes_invoke_with_a_local_error() {
    let (lua, world, sched) = fixture();
    lua.load(
        r#"
        fn = Instance.new("BindableFunction")
        fn.OnInvoke = function() task.wait(5); return "late" end
        task.spawn(function()
            local ok, err = pcall(function() fn:Invoke() end)
            invoke_error = not ok and string.find(err, "destroyed while Invoke was waiting") ~= nil
        end)
    "#,
    )
    .exec()
    .unwrap();
    step(&lua, &sched, Instant::now());
    let bindable_id = world
        .borrow()
        .nodes
        .values()
        .find(|node| node.class_name == "BindableFunction")
        .unwrap()
        .id
        .clone();
    world::destroy_instance(&lua, &world, &bindable_id).unwrap();
    step(&lua, &sched, Instant::now() + Duration::from_secs(6));
    assert_eq!(lua.globals().get::<bool>("invoke_error").unwrap(), true);
}
