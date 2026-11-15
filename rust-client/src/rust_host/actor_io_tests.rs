use super::*;
use crate::rust_host::{scheduler, shared_tables::SharedTableRegistry, world};
use mlua::Lua;
use serde_json::json;
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

fn setup() -> (Lua, WorldRef, SharedTableRegistry) {
    let snapshot = json!({
        "version":1,"sequence":1,
        "services":{"Workspace":"workspace","RunService":"run"},
        "nodes":[
            {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{}},
            {"id":"workspace","class_name":"Workspace","name":"Workspace","parent":"game","properties":{}},
            {"id":"run","class_name":"RunService","name":"RunService","parent":"game","properties":{}},
            {"id":"worker","class_name":"Actor","name":"worker","parent":"workspace","properties":{}},
            {"id":"remote","class_name":"RemoteFunction","name":"F","parent":"workspace","properties":{},"server_owned":true},
            {"id":"event","class_name":"RemoteEvent","name":"E","parent":"workspace","properties":{},"server_owned":true}
        ]
    });
    let world = Rc::new(RefCell::new(
        world::World::from_snapshot(&snapshot, None).unwrap(),
    ));
    let lua = Lua::new();
    let scheduler = Rc::new(RefCell::new(scheduler::Scheduler::new()));
    scheduler::install(&lua, &scheduler).unwrap();
    super::super::install_bindings(&lua, &world).unwrap();
    let pool = lua.app_data_ref::<SharedTableRegistry>().unwrap().clone();
    super::actors::install(&lua, &world, pool.clone()).unwrap();
    (lua, world, pool)
}

fn tick(lua: &Lua, world: &WorldRef) {
    let scheduler = lua
        .app_data_ref::<scheduler::SchedulerRef>()
        .unwrap()
        .clone();
    scheduler::step(lua, &scheduler, Instant::now(), Duration::from_millis(50));
    let _ = world;
}

#[test]
fn actor_calls_get_disjoint_host_ids_and_route_only_matching_results() {
    let (root_lua, root_world, pool) = setup();
    super::actors::ensure_actor(&root_lua, &root_world, &pool, "worker").unwrap();

    root_lua
        .load("task.spawn(function() root_wait=workspace.F:InvokeServer('root') end)")
        .exec()
        .unwrap();
    tick(&root_lua, &root_world);
    let root_active = remotes::active_requests(&root_lua).unwrap();
    assert_eq!(root_active, json!([1]));

    super::actors::with_actor(&root_lua, "worker", |lua, _, _| {
        lua.load("task.spawn(function() workspace.E:FireServer('event'); local a,b=workspace.F:InvokeServer('actor'); actor_result={a,b} end)")
            .exec()?;
        let scheduler = lua.app_data_ref::<scheduler::SchedulerRef>().unwrap().clone();
        scheduler::step(lua, &scheduler, Instant::now(), Duration::from_millis(50));
        Ok(())
    })
    .unwrap()
    .unwrap();

    let effects = super::actors::with_actor(&root_lua, "worker", |_, world, _| {
        Ok(Json::Object(std::mem::take(
            &mut world.borrow_mut().effects,
        )))
    })
    .unwrap()
    .unwrap();
    let mut actor_status = json!([{"actor_id":"worker","world_effects":effects}]);
    let mut router = ActorIoRouter::default();
    let forwarded = router
        .collect_after_step(&root_lua, &root_world, &mut actor_status, &root_active)
        .unwrap();
    let public_id = ACTOR_REQUEST_ID_BASE;
    assert_eq!(forwarded["remote_server_events"][0]["node"], "event");
    assert_eq!(forwarded["remote_server_calls"][0]["request_id"], public_id);
    assert_eq!(
        forwarded["remote_server_calls"][0]["arguments"],
        json!(["actor"])
    );
    assert_eq!(
        forwarded["active_remote_server_requests"],
        json!([1, public_id])
    );
    assert_eq!(
        actor_status[0]["world_effects"]["remote_server_calls"],
        json!([])
    );

    let wrong = router
        .route_server_results(
            &root_lua,
            &[json!({"node":"not-remote","request_id":public_id,"ok":true,"values":[1]})],
        )
        .unwrap();
    assert!(wrong.is_empty());
    assert_eq!(router.pending_routes(), 1);

    let root_rows = router
        .route_server_results(
            &root_lua,
            &[
                json!({"node":"remote","request_id":public_id,"ok":true,"values":[55,"ok"]}),
                json!({"node":"remote","request_id":1,"ok":false,"error":"root error"}),
            ],
        )
        .unwrap();
    assert_eq!(
        root_rows,
        vec![json!({"node":"remote","request_id":1,"ok":false,"error":"root error"})]
    );
    assert_eq!(router.pending_routes(), 0);

    super::actors::with_actor(&root_lua, "worker", |lua, _, scheduler| {
        scheduler::step(lua, scheduler, Instant::now(), Duration::from_millis(50));
        let result: mlua::Table = lua.globals().get("actor_result")?;
        assert_eq!(result.get::<i64>(1)?, 55);
        assert_eq!(result.get::<String>(2)?, "ok");
        Ok(())
    })
    .unwrap()
    .unwrap();
}

#[test]
fn unknown_actor_namespace_results_are_dropped_without_fabricated_completion() {
    let (root_lua, _, _) = setup();
    let mut router = ActorIoRouter::default();
    let result = router
        .route_server_results(
            &root_lua,
            &[
                json!({"node":"remote","request_id":ACTOR_REQUEST_ID_BASE+9,"ok":true,"values":[]}),
                json!({"node":"remote","request_id":7,"ok":false,"error":"root"}),
            ],
        )
        .unwrap();
    assert_eq!(
        result,
        vec![json!({"node":"remote","request_id":7,"ok":false,"error":"root"})]
    );
    assert_eq!(router.pending_routes(), 0);
}

#[test]
fn actor_receives_replicated_invoke_locally_without_duplicating_root_reply() {
    let (root_lua, root_world, pool) = setup();
    super::actors::ensure_actor(&root_lua, &root_world, &pool, "worker").unwrap();
    super::actors::with_actor(&root_lua, "worker", |lua, _, _| {
        lua.load("workspace.F.OnClientInvoke=function(x) return x+1 end")
            .exec()?;
        Ok(())
    })
    .unwrap()
    .unwrap();

    let delta = json!({"version":1,"delta":true,"sequence":5,"actions":[
        {"sequence":5,"kind":"event","id":"remote","name":"RemoteOnInvokeClient","arguments":[41,[2]]}
    ]});
    let mut router = ActorIoRouter::default();
    router.queue_delta_events(&delta, 1).unwrap();
    let mut status = json!([{
        "actor_id":"worker",
        "pending_tasks":0,
        "world_effects":{"remote_server_events":[],"remote_server_calls":[]}
    }]);
    let first = router
        .collect_after_step(&root_lua, &root_world, &mut status, &json!([]))
        .unwrap();
    assert_eq!(first["actor_events_dispatched"], 1);
    assert_eq!(status[0]["pending_tasks"], 1);
    assert!(first.get("remote_client_replies").is_none());

    super::actors::with_actor(&root_lua, "worker", |lua, _, scheduler| {
        scheduler::step(lua, scheduler, Instant::now(), Duration::from_millis(50));
        Ok(())
    })
    .unwrap()
    .unwrap();
    let mut status = json!([{
        "actor_id":"worker",
        "world_effects":{"remote_server_events":[],"remote_server_calls":[]}
    }]);
    let next = router
        .collect_after_step(&root_lua, &root_world, &mut status, &json!([]))
        .unwrap();
    assert_eq!(next["unbridged_actor_remote_client_replies"], 1);
    assert!(next.get("remote_client_replies").is_none());
}
