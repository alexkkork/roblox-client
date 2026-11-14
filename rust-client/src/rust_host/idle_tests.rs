use crate::rust_host::{
    idle,
    scheduler::{self, Scheduler, SchedulerRef},
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

fn fixture() -> WorldRef {
    let snapshot = json!({"version":1,"sequence":0,"local_player":"player",
    "services":{"Players":"players"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{},"server_owned":true},
        {"id":"players","class_name":"Players","name":"Players","parent":"game","properties":{},"server_owned":true},
        {"id":"player","class_name":"Player","name":"LocalPlayer","parent":"players","properties":{},"server_owned":true}
    ]});
    Rc::new(RefCell::new(World::from_snapshot(&snapshot, None).unwrap()))
}

fn install(lua: &Lua, ctx: &WorldRef) -> mlua::Result<SchedulerRef> {
    super::world::install(lua, ctx)?;
    let scheduler = Rc::new(RefCell::new(Scheduler::new()));
    scheduler::install(lua, &scheduler)?;
    signals::install(lua, ctx)?;
    idle::install(lua, ctx)?;
    idle::refresh(lua, ctx, Instant::now())?;
    Ok(scheduler)
}

#[test]
fn timer_uses_first_notification_repeat_cadence_and_activity_reset() {
    let base = Instant::now();
    let mut timer = idle::IdleRuntime::default();
    timer.start("player".into(), base);
    assert!(!timer.poll_idle(base + Duration::from_millis(119_999)));
    assert!(timer.poll_idle(base + Duration::from_millis(120_000)));
    assert!(!timer.poll_idle(base + Duration::from_millis(149_999)));
    assert!(timer.poll_idle(base + Duration::from_millis(150_000)));
    timer.observe_native_idle(base + Duration::from_millis(150_000));
    assert_eq!(timer.notifications, 1);
    timer.activity_at(base + Duration::from_millis(151_000));
    assert!(!timer.event_observed);
    assert!(!timer.poll_idle(base + Duration::from_millis(270_999)));
    assert!(timer.poll_idle(base + Duration::from_millis(271_000)));
}

#[test]
fn only_non_synthetic_user_input_resets_the_idle_clock() -> mlua::Result<()> {
    let lua = Lua::new();
    let ctx = fixture();
    let _scheduler = install(&lua, &ctx)?;
    let player = world::instance(&lua, &ctx, "player")?;
    lua.globals().set("player", player)?;
    lua.load("idle_signal = player.Idled").exec()?;
    let state = idle::idle_state(&lua)?;
    let before = state.borrow().last_activity_ms;
    state.borrow_mut().event_observed = true;

    idle::record_activity(&lua, "Keyboard", "Begin", true);
    assert_eq!(state.borrow().last_activity_ms, before);
    assert!(state.borrow().event_observed);
    idle::record_activity(&lua, "Keyboard", "Cancel", false);
    assert_eq!(state.borrow().last_activity_ms, before);
    idle::record_activity(&lua, "Focus", "Begin", false);
    assert_eq!(state.borrow().last_activity_ms, before);
    idle::record_activity(&lua, "Keyboard", "Begin", false);
    assert!(state.borrow().last_activity_ms >= before);
    assert!(!state.borrow().event_observed);
    assert!(matches!(
        lua.globals().get::<Value>("idle_signal")?,
        Value::UserData(_)
    ));
    Ok(())
}

#[test]
fn local_player_idled_emits_elapsed_seconds_and_report() -> mlua::Result<()> {
    let lua = Lua::new();
    let ctx = fixture();
    let scheduler = install(&lua, &ctx)?;
    let player = world::instance(&lua, &ctx, "player")?;
    lua.globals().set("player", player)?;
    lua.load(
        r#"
        idle_seconds = -1
        player.Idled:Connect(function(seconds) idle_seconds = seconds end)
        native_connection = player.Idled:GetConnections()[1]
    "#,
    )
    .exec()?;
    assert!(lua.load("native_connection.LuaConnection == false and native_connection.Function == nil and native_connection.Connected and native_connection.Enabled").eval::<bool>()?);
    assert_eq!(signals::diagnostics(&lua)?.active_connections, 2);
    let started = idle::idle_state(&lua)?.borrow().started.unwrap();
    signals::fire(
        &lua,
        &ctx,
        "player",
        "Idled",
        mlua::MultiValue::from_vec(vec![Value::Number(999.0)]),
    )?;
    let manual = scheduler::step(&lua, &scheduler, started, Duration::from_millis(50));
    assert!(manual.errors.is_empty(), "{:?}", manual.errors);
    assert_eq!(lua.globals().get::<f64>("idle_seconds")?, 999.0);
    assert_eq!(idle::report(&lua, started)["native_notifications"], 0);
    idle::step(&lua, &ctx, started + Duration::from_secs(120))?;
    let report = scheduler::step(
        &lua,
        &scheduler,
        started + Duration::from_secs(120),
        Duration::from_millis(50),
    );
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert_eq!(lua.globals().get::<f64>("idle_seconds")?, 120.0);
    let idle_report = idle::report(&lua, started + Duration::from_secs(121));
    assert_eq!(idle_report["player_id"], "player");
    assert_eq!(idle_report["event_due"], true);
    assert_eq!(idle_report["elapsed_inactive_ms"], 121_000);
    assert_eq!(idle_report["native_notifications"], 1);
    lua.load("native_connection.Enabled = false").exec()?;
    idle::step(&lua, &ctx, started + Duration::from_secs(150))?;
    assert_eq!(
        idle::report(&lua, started + Duration::from_secs(150))["native_notifications"],
        1
    );
    lua.load("native_connection:Disconnect(); was_disconnected = not native_connection.Connected")
        .exec()?;
    assert!(lua.globals().get::<bool>("was_disconnected")?);
    idle::step(&lua, &ctx, started + Duration::from_secs(180))?;
    assert_eq!(
        idle::report(&lua, started + Duration::from_secs(180))["native_notifications"],
        1
    );
    Ok(())
}
