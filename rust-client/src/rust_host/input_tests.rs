use crate::rust_host::{
    enums, input,
    scheduler::{self, Scheduler, SchedulerRef},
    signals, value_types,
    world::{self, World, WorldRef},
};
use mlua::{Lua, Value};
use serde_json::json;
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

fn fixture() -> WorldRef {
    let snapshot = json!({"version":1,"sequence":0,"input_capabilities":{"KeyboardEnabled":true,"MouseEnabled":true},
        "services":{"UserInputService":"uis","ContextActionService":"cas","VirtualInputManager":"vim","PlayerScripts":"ps"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{},"server_owned":true},
            {"id":"uis","class_name":"UserInputService","name":"UserInputService","parent":"game","properties":{},"server_owned":true},
            {"id":"cas","class_name":"ContextActionService","name":"ContextActionService","parent":"game","properties":{},"server_owned":true},
            {"id":"vim","class_name":"VirtualInputManager","name":"VirtualInputManager","parent":"game","properties":{},"server_owned":true},
            {"id":"ps","class_name":"PlayerScripts","name":"PlayerScripts","parent":"game","properties":{},"server_owned":true}
    ]});
    Rc::new(RefCell::new(World::from_snapshot(&snapshot, None).unwrap()))
}

fn install(lua: &Lua, ctx: &WorldRef) -> mlua::Result<SchedulerRef> {
    enums::install(lua)?;
    value_types::install(lua)?;
    super::super::world::install(lua, ctx)?;
    let scheduler = Rc::new(RefCell::new(Scheduler::new()));
    scheduler::install(lua, &scheduler)?;
    signals::install(lua, ctx)?;
    input::install(lua, ctx)?;
    Ok(scheduler)
}

#[test]
fn synthetic_keyboard_events_keep_typed_objects_and_ordered_callbacks() -> mlua::Result<()> {
    let lua = Lua::new();
    let ctx = fixture();
    let scheduler = install(&lua, &ctx)?;
    let uis = super::super::world::instance(&lua, &ctx, "uis")?;
    lua.globals().set("uis", uis)?;
    lua.load(r#"
        order = {}
        uis.LastInputTypeChanged:Connect(function(value) table.insert(order, "last:" .. value.Name) end)
        uis.InputBegan:Connect(function(input, processed)
            table.insert(order, "begin:" .. input.KeyCode.Name .. ":" .. tostring(processed))
            saved = input
        end)
        uis.InputEnded:Connect(function(input, processed)
            table.insert(order, "end:" .. input.UserInputState.Name .. ":" .. tostring(processed))
        end)
    "#).exec()?;
    input::action(
        &lua,
        &ctx,
        &json!({"events":[{"phase":"begin","input_type":"Keyboard","key_code":"W","processed":false}]}),
    )?;
    let started = scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(50));
    assert!(started.errors.is_empty(), "{:?}", started.errors);
    assert!(lua.load("uis:IsKeyDown(Enum.KeyCode.W) and #uis:GetKeysPressed() == 1 and saved.KeyCode == Enum.KeyCode.W").eval::<bool>()?);
    input::action(
        &lua,
        &ctx,
        &json!({"events":[{"phase":"end","input_type":"Keyboard","key_code":"W","processed":true}]}),
    )?;
    let ended = scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(50));
    assert!(ended.errors.is_empty(), "{:?}", ended.errors);
    assert!(!lua.load("uis:IsKeyDown(Enum.KeyCode.W)").eval::<bool>()?);
    let order: mlua::Table = lua.globals().get("order")?;
    assert_eq!(order.get::<String>(1)?, "last:Keyboard");
    assert_eq!(order.get::<String>(2)?, "begin:W:false");
    assert_eq!(order.get::<String>(3)?, "end:End:true");
    assert!(matches!(
        lua.globals().get::<Value>("saved")?,
        Value::UserData(_)
    ));
    Ok(())
}

#[test]
fn input_action_rejects_bad_phases_and_malformed_vectors() -> mlua::Result<()> {
    let lua = Lua::new();
    let ctx = fixture();
    let _ = install(&lua, &ctx)?;
    assert!(input::action(
        &lua,
        &ctx,
        &json!({"events":[{"phase":"begin","input_type":"MouseWheel","position":[0,0,1]}]})
    )
    .is_err());
    assert!(input::action(
        &lua,
        &ctx,
        &json!({"events":[{"phase":"change","input_type":"MouseMovement","position":[0,0]}]})
    )
    .is_err());
    Ok(())
}

#[test]
fn context_actions_are_priority_ordered_and_sink_lower_bindings() -> mlua::Result<()> {
    let lua = Lua::new();
    let ctx = fixture();
    let scheduler = install(&lua, &ctx)?;
    let cas = world::instance(&lua, &ctx, "cas")?;
    lua.globals().set("cas", cas)?;
    lua.load(r#"
        order = {}
        cas:BindAction("low", function(name, state, input) table.insert(order, "low") end, false, Enum.KeyCode.W)
        cas:BindActionAtPriority("high", function(name, state, input)
            table.insert(order, name .. ":" .. state.Name .. ":" .. input.KeyCode.Name)
            return Enum.ContextActionResult.Sink
        end, false, 3000, Enum.KeyCode.W)
    "#).exec()?;
    input::action(
        &lua,
        &ctx,
        &json!({"events":[{"phase":"begin","input_type":"Keyboard","key_code":"W","processed":false}]}),
    )?;
    let report = scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(50));
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let order: mlua::Table = lua.globals().get("order")?;
    assert_eq!(order.get::<String>(1)?, "high:Begin:W");
    assert_eq!(order.raw_len(), 1);
    Ok(())
}

#[test]
fn virtual_input_manager_sends_only_local_vm_events() -> mlua::Result<()> {
    let lua = Lua::new();
    let ctx = fixture();
    let scheduler = install(&lua, &ctx)?;
    let uis = world::instance(&lua, &ctx, "uis")?;
    lua.globals().set("uis", uis)?;
    let vim = world::instance(&lua, &ctx, "vim")?;
    lua.globals().set("vim", vim)?;
    lua.load("vim:SendKeyEvent(true, Enum.KeyCode.W, false, nil)")
        .exec()?;
    assert!(lua.load("uis:IsKeyDown(Enum.KeyCode.W)").eval::<bool>()?);
    lua.load("vim:SendKeyEvent(false, Enum.KeyCode.W, false, nil)")
        .exec()?;
    assert!(!lua.load("uis:IsKeyDown(Enum.KeyCode.W)").eval::<bool>()?);
    let report = scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(50));
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    Ok(())
}

#[test]
fn movement_mode_first_registration_resets_defaults_and_clear_keeps_empty() -> mlua::Result<()> {
    let lua = Lua::new();
    let ctx = fixture();
    let _ = install(&lua, &ctx)?;
    let ps = world::instance(&lua, &ctx, "ps")?;
    lua.globals().set("ps", ps)?;
    lua.load(
        r#"
        ps:RegisterTouchMovementMode(Enum.TouchMovementMode.ClickToMove)
        modes = ps:GetRegisteredTouchMovementModes()
        ps:ClearTouchMovementModes()
        cleared = ps:GetRegisteredTouchMovementModes()
    "#,
    )
    .exec()?;
    let modes: mlua::Table = lua.globals().get("modes")?;
    assert_eq!(modes.raw_len(), 1);
    assert!(lua
        .load("modes[1] == Enum.TouchMovementMode.ClickToMove")
        .eval::<bool>()?);
    let cleared: mlua::Table = lua.globals().get("cleared")?;
    assert_eq!(cleared.raw_len(), 0);
    Ok(())
}

#[test]
fn virtual_input_recording_replays_on_timestamps_and_releases_owned_keys() -> mlua::Result<()> {
    let lua = Lua::new();
    let ctx = fixture();
    let scheduler = install(&lua, &ctx)?;
    let vim = world::instance(&lua, &ctx, "vim")?;
    let uis = world::instance(&lua, &ctx, "uis")?;
    lua.globals().set("vim", vim)?;
    lua.globals().set("uis", uis)?;
    lua.load(
        r#"
        completed = false
        vim.PlaybackCompleted:Connect(function(state)
            completed = state == "playback-state"
        end)
    "#,
    )
    .exec()?;

    let tape = json!({
        "format":"rove.virtual-input", "version":1, "duration":1.0,
        "additionalLuaState":"playback-state", "initialMousePosition":[0,0],
        "events":[
            {"time":0.25,"method":"SendKeyEvent","args":[true,{"$enum":{"type":"KeyCode","name":"W","value":17}},false]},
            {"time":0.75,"method":"SendKeyEvent","args":[false,{"$enum":{"type":"KeyCode","name":"W","value":17}},false]}
        ]
    });
    lua.globals().set("tape", tape.to_string())?;
    lua.load("vim:StartPlayingJSON(tape)").exec()?;
    let start = input::state(&lua)?.borrow().playback_started_at.unwrap();
    let vm_started = start;

    input::step(&lua, &ctx, start + Duration::from_millis(200), vm_started)?;
    assert!(!lua.load("uis:IsKeyDown(Enum.KeyCode.W)").eval::<bool>()?);
    input::step(&lua, &ctx, start + Duration::from_millis(300), vm_started)?;
    assert!(lua.load("uis:IsKeyDown(Enum.KeyCode.W)").eval::<bool>()?);
    input::step(&lua, &ctx, start + Duration::from_millis(800), vm_started)?;
    assert!(!lua.load("uis:IsKeyDown(Enum.KeyCode.W)").eval::<bool>()?);
    input::step(&lua, &ctx, start + Duration::from_millis(1100), vm_started)?;
    let report = scheduler::step(
        &lua,
        &scheduler,
        start + Duration::from_millis(1100),
        Duration::from_millis(50),
    );
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(lua.load("completed").eval::<bool>()?);
    Ok(())
}

#[test]
fn virtual_input_playback_cancel_releases_only_its_held_key() -> mlua::Result<()> {
    let lua = Lua::new();
    let ctx = fixture();
    let _scheduler = install(&lua, &ctx)?;
    let vim = world::instance(&lua, &ctx, "vim")?;
    let uis = world::instance(&lua, &ctx, "uis")?;
    lua.globals().set("vim", vim)?;
    lua.globals().set("uis", uis)?;
    lua.load("vim:SendKeyEvent(true, Enum.KeyCode.A, false)")
        .exec()?;
    let tape = json!({
        "format":"rove.virtual-input", "version":1, "duration":2.0,
        "additionalLuaState":"", "events":[
            {"time":0.0,"method":"SendKeyEvent","args":[true,{"$enum":{"type":"KeyCode","name":"W","value":17}},false]}
        ]
    });
    lua.globals().set("tape", tape.to_string())?;
    lua.load("vim:StartPlayingJSON(tape)").exec()?;
    let start = input::state(&lua)?.borrow().playback_started_at.unwrap();
    input::step(&lua, &ctx, start + Duration::from_millis(1), start)?;
    assert!(lua.load("uis:IsKeyDown(Enum.KeyCode.W)").eval::<bool>()?);
    lua.load("vim:StopPlaying()").exec()?;
    assert!(!lua.load("uis:IsKeyDown(Enum.KeyCode.W)").eval::<bool>()?);
    assert!(lua.load("uis:IsKeyDown(Enum.KeyCode.A)").eval::<bool>()?);
    assert!(input::state(&lua)?.borrow().playback_started_at.is_none());
    Ok(())
}
