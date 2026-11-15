use super::{
    errors,
    scheduler::{self, Scheduler},
    signals,
    world::{World, WorldRef},
};
use mlua::{Lua, MultiValue, Value};
use serde_json::json;
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

fn fixture() -> (Lua, WorldRef, scheduler::SchedulerRef) {
    let lua = Lua::new();
    let snapshot = json!({"version":1,"sequence":0,"nodes":[]});
    let world = Rc::new(RefCell::new(World::from_snapshot(&snapshot, None).unwrap()));
    let sched = Rc::new(RefCell::new(Scheduler::new()));
    scheduler::install(&lua, &sched).unwrap();
    errors::install(&lua).unwrap();
    signals::install(&lua, &world).unwrap();
    (lua, world, sched)
}

fn tick(lua: &Lua, sched: &scheduler::SchedulerRef) -> scheduler::StepReport {
    scheduler::step(lua, sched, Instant::now(), Duration::from_millis(100))
}

#[test]
fn callbacks_are_queued_in_registration_order_and_receive_the_exact_tuple() {
    let (lua, world, sched) = fixture();
    let signal = signals::signal(&lua, &world, "local:source", "event").unwrap();
    lua.globals().set("event", signal).unwrap();
    lua.load(r##"
        received = {}
        event:Connect(function(a, b, c) table.insert(received, "first"); received.a=a; received.b=b; received.c=c end)
        event:Connect(function() table.insert(received, "second") end)
    "##).exec().unwrap();
    let args: MultiValue = lua.load("return 'x', nil, 7").eval().unwrap();
    signals::fire(&lua, &world, "local:source", "event", args).unwrap();
    assert_eq!(tick(&lua, &sched).resumed, 2);
    assert_eq!(
        lua.load("return received[1], received[2], received.a, received.b, received.c")
            .eval::<(String, String, String, mlua::Value, i64)>()
            .unwrap(),
        (
            "first".into(),
            "second".into(),
            "x".into(),
            mlua::Value::Nil,
            7
        )
    );
}

#[test]
fn signal_diagnostics_count_live_listeners_and_waiters_only() {
    let (lua, world, sched) = fixture();
    let signal = signals::signal(&lua, &world, "stats", "event").unwrap();
    lua.globals().set("event", signal).unwrap();
    lua.load("event:Connect(function() end); task.spawn(function() event:Wait() end)")
        .exec()
        .unwrap();
    tick(&lua, &sched);
    let snapshot = signals::diagnostics(&lua).unwrap();
    assert_eq!(snapshot.active_signal_keys, 1);
    assert_eq!(snapshot.active_connections, 1);
    assert_eq!(snapshot.waiting_coroutines, 1);
    assert_eq!(snapshot.connection_limit_per_signal, 4096);

    signals::fire(&lua, &world, "stats", "event", MultiValue::new()).unwrap();
    tick(&lua, &sched);
    let snapshot = signals::diagnostics(&lua).unwrap();
    assert_eq!(snapshot.waiting_coroutines, 0);
    assert_eq!(snapshot.active_connections, 1);
    signals::cleanup_instance(&lua, &world, "stats").unwrap();
    let snapshot = signals::diagnostics(&lua).unwrap();
    assert_eq!(snapshot.active_connections, 0);
    assert_eq!(snapshot.destroyed_instances, 1);
}

#[test]
fn once_disconnect_and_enabled_properties_have_expected_lifecycle() {
    let (lua, world, sched) = fixture();
    let signal = signals::signal(&lua, &world, "opaque", "tick").unwrap();
    lua.globals().set("event", signal).unwrap();
    lua.load(
        r#"
        calls = 0
        once_connection = event:Once(function() calls += 1 end)
        ordinary = event:Connect(function() calls += 10 end)
        ordinary.Enabled = false
        assert(ordinary.Connected == true and ordinary.Enabled == false)
    "#,
    )
    .exec()
    .unwrap();
    signals::fire(&lua, &world, "opaque", "tick", MultiValue::new()).unwrap();
    assert_eq!(tick(&lua, &sched).resumed, 1);
    assert_eq!(
        lua.load("return calls, once_connection.Connected, ordinary.Connected")
            .eval::<(i64, bool, bool)>()
            .unwrap(),
        (1, false, true)
    );
    lua.load("ordinary:Disconnect(); assert(not ordinary.Connected)")
        .exec()
        .unwrap();
    signals::fire(&lua, &world, "opaque", "tick", MultiValue::new()).unwrap();
    assert_eq!(tick(&lua, &sched).resumed, 0);
    assert_eq!(lua.globals().get::<i64>("calls").unwrap(), 1);
}

#[test]
fn wait_parks_and_resumes_a_local_coroutine_with_all_event_values() {
    let (lua, world, sched) = fixture();
    let signal = signals::signal(&lua, &world, "opaque", "waitable").unwrap();
    lua.globals().set("event", signal).unwrap();
    lua.load(
        r##"
        waited = nil
        task.spawn(function()
            local a, b, c = event:Wait()
            waited = {a, b, c, n=select("#", a, b, c)}
        end)
    "##,
    )
    .exec()
    .unwrap();
    assert_eq!(tick(&lua, &sched).resumed, 1);
    assert_eq!(
        lua.load("return waited == nil").eval::<bool>().unwrap(),
        true
    );
    let args: MultiValue = lua.load("return 'wake', nil, 9").eval().unwrap();
    signals::fire(&lua, &world, "opaque", "waitable", args).unwrap();
    assert_eq!(tick(&lua, &sched).resumed, 1);
    assert_eq!(
        lua.load("return waited[1], waited[2], waited[3], waited.n")
            .eval::<(String, mlua::Value, i64, i64)>()
            .unwrap(),
        ("wake".into(), mlua::Value::Nil, 9, 3)
    );
}

#[test]
fn host_owned_connections_can_be_cleaned_up_without_touching_other_listeners() {
    let (lua, world, sched) = fixture();
    let callback: mlua::Function = lua
        .load("return function() owned=owned+1 end")
        .eval()
        .unwrap();
    lua.globals().set("owned", 0).unwrap();
    signals::connect_owned(&lua, &world, "x", "y", callback, "job-1", false).unwrap();
    let signal = signals::signal(&lua, &world, "x", "y").unwrap();
    lua.globals().set("event", signal).unwrap();
    lua.load("event:Connect(function() local_listener=1 end)")
        .exec()
        .unwrap();
    assert_eq!(signals::cleanup_owner(&lua, "job-1").unwrap(), 1);
    signals::fire(&lua, &world, "x", "y", MultiValue::new()).unwrap();
    assert_eq!(tick(&lua, &sched).resumed, 1);
    assert_eq!(
        lua.load("return owned, local_listener")
            .eval::<(i64, i64)>()
            .unwrap(),
        (0, 1)
    );
}

#[test]
fn instance_cleanup_disconnects_callbacks_and_wakes_waiters() {
    let (lua, world, sched) = fixture();
    let signal = signals::signal(&lua, &world, "removed", "event").unwrap();
    lua.globals().set("event", signal).unwrap();
    lua.load(
        r#"
        callback_ran = false
        connection = event:Connect(function() callback_ran = true end)
        task.spawn(function()
            local packed = table.pack(event:Wait())
            wait_result_count = packed.n
        end)
    "#,
    )
    .exec()
    .unwrap();
    tick(&lua, &sched);
    assert_eq!(
        signals::cleanup_instance(&lua, &world, "removed").unwrap(),
        1
    );
    tick(&lua, &sched);
    assert_eq!(
        lua.load("return connection.Connected, wait_result_count, #event:GetConnections()")
            .eval::<(bool, i64, i64)>()
            .unwrap(),
        (false, 0, 0)
    );
    lua.load(
        r#"
        local ok, err = pcall(function() event:Connect(function() end) end)
        cleanup_rejects_connection = not ok and string.find(err, "destroyed local instance") ~= nil
    "#,
    )
    .exec()
    .unwrap();
    assert_eq!(
        lua.globals()
            .get::<bool>("cleanup_rejects_connection")
            .unwrap(),
        true
    );
}

#[test]
fn compatibility_globals_preserve_order_args_and_native_only_connections() {
    let (lua, world, sched) = fixture();
    let signal = signals::signal(&lua, &world, "compat-source", "Changed").unwrap();
    lua.globals().set("event", signal).unwrap();
    lua.load(
        r#"
        calls = {}
        first = event:Connect(function(a,b,c)
            table.insert(calls, "first")
            first_args = {a=a,b=b,c=c}
        end)
        second = event:Once(function() table.insert(calls, "second") end)
    "#,
    )
    .exec()
    .unwrap();
    let native_id =
        signals::connect_native(&lua, &world, "compat-source", "Changed", "engine-idle").unwrap();
    lua.load(r#"
        local connections = getconnections(event)
        assert(#connections == 3)
        assert(connections[1].Connected and not connections[1].LuaConnection)
        assert(connections[1].Function == nil)
        assert(connections[2].Connected and connections[2].LuaConnection and type(connections[2].Function) == "function")
        firesignal(event, "payload", nil, 7)
    "#).exec().unwrap();
    assert_eq!(tick(&lua, &sched).resumed, 2);
    assert_eq!(
        lua.load(r#"return calls[1], calls[2], first_args.a, first_args.b, first_args.c"#)
            .eval::<(String, String, String, Value, i64)>()
            .unwrap(),
        (
            "first".into(),
            "second".into(),
            "payload".into(),
            Value::Nil,
            7
        )
    );
    lua.load(
        r#"
        local connections = getconnections(event)
        assert(#connections == 2)
        assert(connections[1].Function == nil and not connections[1].LuaConnection)
        connections[1]:Disable()
        assert(not connections[1].Enabled)
        connections[1]:Enable()
        assert(connections[1].Enabled)
        assert(connections[1].Connected)
        connections[1]:Disconnect()
        assert(not connections[1].Connected)
    "#,
    )
    .exec()
    .unwrap();
    assert_eq!(
        signals::owned_status(&lua, &world, "compat-source", "Changed", "engine-idle").unwrap(),
        None
    );
    assert!(
        signals::disconnect_owned(&lua, &world, "compat-source", "Changed", "engine-idle").is_ok()
    );
    let _ = native_id;
}

#[test]
fn compatibility_firesignal_resumes_waiters_and_ignores_native_only_records() {
    let (lua, world, sched) = fixture();
    let signal = signals::signal(&lua, &world, "compat-wait", "Event").unwrap();
    lua.globals().set("event", signal).unwrap();
    lua.load(
        r#"
        wait_result = nil
        task.spawn(function()
            local a,b = event:Wait()
            wait_result = {a=a,b=b}
        end)
    "#,
    )
    .exec()
    .unwrap();
    tick(&lua, &sched);
    signals::connect_native(&lua, &world, "compat-wait", "Event", "native-only").unwrap();
    lua.load("firesignal(event, 'ready', nil)").exec().unwrap();
    tick(&lua, &sched);
    assert_eq!(
        lua.load("return wait_result.a, wait_result.b")
            .eval::<(String, Value)>()
            .unwrap(),
        ("ready".into(), Value::Nil)
    );
    assert_eq!(
        lua.load("local c=getconnections(event); return #c,c[1].Function,c[1].LuaConnection")
            .eval::<(i64, Value, bool)>()
            .unwrap(),
        (1, Value::Nil, false)
    );
}

#[test]
fn connection_thread_is_the_creator_and_callbacks_inherit_its_context() {
    let (lua, world, sched) = fixture();
    let signal = signals::signal(&lua, &world, "creator", "Changed").unwrap();
    lua.globals().set("event", signal).unwrap();
    lua.load(
        r#"
        task.spawn(function()
            setthreadidentity(6)
            local creator = coroutine.running()
            connection = event:Connect(function() callback_identity = getthreadidentity() end)
            connection_thread_matches = connection.Thread == creator
        end)
    "#,
    )
    .exec()
    .unwrap();
    tick(&lua, &sched);
    assert!(lua
        .globals()
        .get::<bool>("connection_thread_matches")
        .unwrap());

    // Fire from the host's idle context. The scheduled listener should still
    // receive the tracked identity of the coroutine that connected it.
    signals::fire(&lua, &world, "creator", "Changed", MultiValue::new()).unwrap();
    tick(&lua, &sched);
    assert_eq!(lua.globals().get::<i64>("callback_identity").unwrap(), 6);

    signals::connect_native(&lua, &world, "creator", "Changed", "native").unwrap();
    let native_thread: mlua::Value = lua
        .load("local rows=getconnections(event); return rows[1].Thread")
        .eval()
        .unwrap();
    assert_eq!(native_thread, mlua::Value::Nil);
}
