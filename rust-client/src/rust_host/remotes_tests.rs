use crate::rust_host::{
    scheduler::{self, Scheduler},
    signals,
    world::{self, World},
};
use mlua::Lua;
use serde_json::json;
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

fn runtime() -> (Lua, world::WorldRef, scheduler::SchedulerRef) {
    let snapshot = json!({"version":1,"sequence":1,"services":{"Workspace":"ws"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{},"server_owned":true},
        {"id":"ws","class_name":"Workspace","name":"Workspace","parent":"game","properties":{},"server_owned":true},
        {"id":"event","class_name":"RemoteEvent","name":"Event","parent":"ws","properties":{},"server_owned":true},
        {"id":"function","class_name":"RemoteFunction","name":"Function","parent":"ws","properties":{},"server_owned":true}
    ]});
    let ctx = Rc::new(RefCell::new(World::from_snapshot(&snapshot, None).unwrap()));
    let lua = Lua::new();
    let scheduler = Rc::new(RefCell::new(Scheduler::new()));
    lua.set_app_data(scheduler.clone());
    scheduler::install(&lua, &scheduler).unwrap();
    signals::install(&lua, &ctx).unwrap();
    world::install(&lua, &ctx).unwrap();
    super::install(&lua, &ctx).unwrap();
    (lua, ctx, scheduler)
}

#[test]
fn fires_event_with_bounded_remote_table_shapes_and_drains_once() {
    let (lua, ctx, _) = runtime();
    lua.load("workspace.Event:FireServer('hi', {1, 2}, {x = true}, {})")
        .exec()
        .unwrap();
    let events = super::drain_outbound(&ctx);
    assert_eq!(
        events["remote_server_events"],
        json!([{
            "node":"event","arguments":["hi",{"$remote_array":[1.0,2.0]},
            {"$remote_map":[["x",true]]},{"$remote_array":[]}]
        }])
    );
    assert_eq!(
        super::drain_outbound(&ctx)["remote_server_events"],
        json!([])
    );
}

#[test]
fn server_invoke_parks_then_resumes_with_the_exact_result_tuple() {
    let (lua, ctx, scheduler_ref) = runtime();
    lua.load("result = {}; task.spawn(function() local a,b = workspace.Function:InvokeServer('q'); result={a,b} end)").exec().unwrap();
    scheduler::step(
        &lua,
        &scheduler_ref,
        Instant::now(),
        Duration::from_millis(50),
    );
    let queued = super::drain_outbound(&ctx);
    let call = &queued["remote_server_calls"][0];
    assert_eq!(call["node"], "function");
    assert_eq!(call["request_id"], 1);
    assert_eq!(call["arguments"], json!(["q"]));
    assert_eq!(
        super::complete_server_results(
            &lua,
            &ctx,
            &[json!({
                "node":"function","request_id":1,"ok":true,"values":[7,"done"]
            })]
        )
        .unwrap(),
        1
    );
    scheduler::step(
        &lua,
        &scheduler_ref,
        Instant::now(),
        Duration::from_millis(50),
    );
    let result: mlua::Table = lua.globals().get("result").unwrap();
    let value: (i64, String) = (result.get(1).unwrap(), result.get(2).unwrap());
    assert_eq!(value, (7, "done".into()));
}

#[test]
fn server_invoke_rejects_mismatched_receiver_and_fails_only_matching_pending_id() {
    let (lua, ctx, scheduler_ref) = runtime();
    lua.load("task.spawn(function() workspace.Function:InvokeServer() end)")
        .exec()
        .unwrap();
    scheduler::step(
        &lua,
        &scheduler_ref,
        Instant::now(),
        Duration::from_millis(50),
    );
    assert_eq!(
        super::complete_server_results(
            &lua,
            &ctx,
            &[json!({
                "node":"event","request_id":1,"ok":true,"values":[]
            })]
        )
        .unwrap(),
        0
    );
    assert_eq!(
        super::complete_server_results(
            &lua,
            &ctx,
            &[json!({
                "node":"function","request_id":1,"ok":false,"error":"no server reply"
            })]
        )
        .unwrap(),
        1
    );
}

#[test]
fn incoming_server_callback_runs_and_serializes_real_results() {
    let (lua, ctx, scheduler_ref) = runtime();
    lua.load("workspace.Function.OnClientInvoke = function(a, b) task.wait(0); return a+b, nil, {ok=true} end")
        .exec()
        .unwrap();
    super::queue_client_invocation(&lua, &ctx, "function", 41, json!([2, 3])).unwrap();
    super::dispatch_client_invocations(&lua, &ctx).unwrap();
    scheduler::step(
        &lua,
        &scheduler_ref,
        Instant::now(),
        Duration::from_millis(50),
    );
    scheduler::step(
        &lua,
        &scheduler_ref,
        Instant::now() + Duration::from_millis(1),
        Duration::from_millis(50),
    );
    let replies = super::collect_client_invocations(&lua, &ctx, &scheduler_ref).unwrap();
    assert_eq!(
        replies,
        vec![json!({"node":"function","request_id":41,"ok":true,
        "values":[5.0,null,{"$remote_map":[["ok",true]]}]})]
    );
}

#[test]
fn incoming_server_callback_error_preserves_raw_multiline_message() {
    let (lua, ctx, scheduler_ref) = runtime();
    lua.load("workspace.Function.OnClientInvoke = function() error('expected failure\\nsecond line') end")
        .set_name("=persistent-source")
        .exec()
        .unwrap();
    super::queue_client_invocation(&lua, &ctx, "function", 42, json!([])).unwrap();
    super::dispatch_client_invocations(&lua, &ctx).unwrap();
    scheduler::step(
        &lua,
        &scheduler_ref,
        Instant::now(),
        Duration::from_millis(50),
    );
    let replies = super::collect_client_invocations(&lua, &ctx, &scheduler_ref).unwrap();
    assert_eq!(
        replies,
        vec![json!({
            "node":"function",
            "request_id":42,
            "ok":false,
            "error":"persistent-source:1: expected failure\nsecond line"
        })]
    );
}

#[test]
fn callback_error_conversion_does_not_execute_authored_metamethods_in_host_drain() {
    let (lua, ctx, scheduler_ref) = runtime();
    lua.load(
        r#"
        tostring_called = false
        workspace.Function.OnClientInvoke = function()
            error(setmetatable({}, {__tostring = function()
                tostring_called = true
                error('must remain inside the scheduler')
            end}))
        end
    "#,
    )
    .exec()
    .unwrap();
    super::queue_client_invocation(&lua, &ctx, "function", 43, json!([])).unwrap();
    super::dispatch_client_invocations(&lua, &ctx).unwrap();
    scheduler::step(
        &lua,
        &scheduler_ref,
        Instant::now(),
        Duration::from_millis(50),
    );
    let replies = super::collect_client_invocations(&lua, &ctx, &scheduler_ref).unwrap();
    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0]["ok"], false);
    assert_eq!(replies[0]["request_id"], 43);
    assert_eq!(
        replies[0]["error"],
        "RemoteFunction callback failed with a non-string error"
    );
    assert!(!lua.globals().get::<bool>("tostring_called").unwrap());
}

#[test]
fn replication_events_fire_remote_signals_and_queue_remote_function_invokes() {
    let (lua, ctx, scheduler_ref) = runtime();
    lua.load("workspace.Event.OnClientEvent:Connect(function(value) received=value end); workspace.Function.OnClientInvoke=function(x) return x+1 end")
        .exec().unwrap();
    let delta = json!({"version":1,"delta":true,"sequence":5,"actions":[
        {"sequence":4,"kind":"event","id":"event","name":"OnClientEvent","arguments":["hello"]},
        {"sequence":5,"kind":"event","id":"function","name":"RemoteOnInvokeClient","arguments":[41,[2]]}
    ]});
    let report = super::process_delta_events(&lua, &ctx, &delta, 1).unwrap();
    assert_eq!(report.received, 2);
    assert_eq!(report.queued_invocations, 1);
    super::dispatch_client_invocations(&lua, &ctx).unwrap();
    scheduler::step(
        &lua,
        &scheduler_ref,
        Instant::now(),
        Duration::from_millis(50),
    );
    let replies = super::collect_client_invocations(&lua, &ctx, &scheduler_ref).unwrap();
    assert_eq!(
        replies,
        vec![json!({"node":"function","request_id":41,"ok":true,"values":[3.0]})]
    );
    assert_eq!(lua.globals().get::<String>("received").unwrap(), "hello");
    assert_eq!(
        super::process_delta_events(&lua, &ctx, &delta, 5)
            .unwrap()
            .received,
        0
    );
}
