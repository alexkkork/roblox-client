use super::*;
use crate::rust_host::{scheduler, world::World};
use serde_json::json;

fn session(snapshot: &Json) -> (Lua, WorldRef) {
    let mut snapshot = snapshot.clone();
    let fields = snapshot
        .as_object_mut()
        .expect("world test snapshot must be an object");
    fields.entry("version").or_insert(json!(1));
    fields.entry("sequence").or_insert(json!(0));
    let world = Rc::new(RefCell::new(World::from_snapshot(&snapshot, None).unwrap()));
    let lua = Lua::new();
    super::super::integrity::install(&lua).unwrap();
    scheduler::install(&lua, &Rc::new(RefCell::new(scheduler::Scheduler::new()))).unwrap();
    super::super::world::install(&lua, &world).unwrap();
    super::super::source_helpers::install(&lua, &world).unwrap();
    install(&lua, &world).unwrap();
    (lua, world)
}

fn run_scheduler(lua: &Lua) -> Vec<String> {
    let scheduler = lua
        .app_data_ref::<scheduler::SchedulerRef>()
        .unwrap()
        .clone();
    let mut errors = Vec::new();
    for _ in 0..3 {
        let report = scheduler::step(
            lua,
            &scheduler,
            std::time::Instant::now() + std::time::Duration::from_secs(1),
            std::time::Duration::from_millis(30),
        );
        errors.extend(report.errors);
    }
    errors.extend(scheduler::drain_errors(&scheduler));
    errors
}

#[test]
fn module_require_caches_local_module_results() {
    let snapshot = json!({
        "nodes": [
            {"id":"root","class_name":"DataModel","name":"game","server_owned":true},
            {"id":"module1","class_name":"ModuleScript","name":"Config","parent":"root","server_owned":true}
        ],
        "script_sources": {"module1":{"source":"getgenv().module_runs=(getgenv().module_runs or 0)+1; return {answer=42}"}}
    });
    let (lua, world) = session(&snapshot);
    let module = world::instance(&lua, &world, "module1").unwrap();
    lua.globals().set("config_module", module).unwrap();
    let task: Function = lua
        .load("return function() local a=require(config_module); local b=require(config_module); getgenv().module_answer=a==b and a.answer end")
        .eval()
        .unwrap();
    scheduler::enqueue(&lua, task, MultiValue::new()).unwrap();
    let scheduler_errors = run_scheduler(&lua);
    let answer = lua
        .load("return getgenv().module_answer")
        .eval::<Value>()
        .unwrap();
    let status = lua
        .load("local s=client.scriptStatus(); return s.modules_loaded, s.modules_loading, s.modules_failed, s.details[1] and s.details[1].error")
        .eval::<MultiValue>()
        .unwrap();
    assert!(
        matches!(answer, Value::Integer(42)),
        "answer={answer:?}, status={status:?}, scheduler_errors={scheduler_errors:?}"
    );
    assert_eq!(
        lua.load("return getgenv().module_runs")
            .eval::<i64>()
            .unwrap(),
        1
    );
    assert_eq!(
        lua.load("return #getloadedmodules()")
            .eval::<i64>()
            .unwrap(),
        1
    );
}

#[test]
fn require_waits_across_scheduler_polls_and_wakes_concurrent_callers() {
    let snapshot = json!({
        "nodes": [
            {"id":"root","class_name":"DataModel","name":"game","server_owned":true},
            {"id":"module1","class_name":"ModuleScript","name":"Slow","parent":"root","server_owned":true}
        ],
        "script_sources": {"module1":{"source":"task.wait(0); getgenv().module_runs=(getgenv().module_runs or 0)+1; return {answer=99}"}}
    });
    let (lua, world) = session(&snapshot);
    lua.globals()
        .set(
            "slow_module",
            world::instance(&lua, &world, "module1").unwrap(),
        )
        .unwrap();
    let caller: Function = lua
        .load(
            "return function() local value=require(slow_module); getgenv().answer=value.answer end",
        )
        .eval()
        .unwrap();
    scheduler::enqueue(&lua, caller.clone(), MultiValue::new()).unwrap();
    scheduler::enqueue(&lua, caller, MultiValue::new()).unwrap();
    let scheduler_ref = lua
        .app_data_ref::<scheduler::SchedulerRef>()
        .unwrap()
        .clone();
    scheduler::step(
        &lua,
        &scheduler_ref,
        std::time::Instant::now(),
        std::time::Duration::from_millis(30),
    );
    assert_eq!(
        lua.load("return getgenv().answer").eval::<Value>().unwrap(),
        Value::Nil
    );
    let scheduler_errors = run_scheduler(&lua);
    let answer = lua.load("return getgenv().answer").eval::<Value>().unwrap();
    let status = lua
        .load("local s=client.scriptStatus(); return s.modules_loaded, s.modules_loading, s.modules_failed, s.details[1] and s.details[1].error")
        .eval::<MultiValue>()
        .unwrap();
    assert!(
        matches!(answer, Value::Integer(99)),
        "answer={answer:?}, status={status:?}, scheduler_errors={scheduler_errors:?}"
    );
    assert_eq!(
        lua.load("return getgenv().module_runs")
            .eval::<i64>()
            .unwrap(),
        1
    );
}

#[test]
fn require_reports_nested_circular_module_dependencies() {
    let snapshot = json!({
        "nodes": [
            {"id":"root","class_name":"DataModel","name":"game","server_owned":true},
            {"id":"module_a","class_name":"ModuleScript","name":"A","parent":"root","server_owned":true},
            {"id":"module_b","class_name":"ModuleScript","name":"B","parent":"root","server_owned":true}
        ],
        "script_sources": {
            "module_a":{"source":"require(module_b_value); return true"},
            "module_b":{"source":"require(module_a_value); return true"}
        }
    });
    let (lua, world) = session(&snapshot);
    let a = world::instance(&lua, &world, "module_a").unwrap();
    let b = world::instance(&lua, &world, "module_b").unwrap();
    lua.globals().set("module_a_value", a).unwrap();
    lua.globals().set("module_b_value", b).unwrap();
    let task: Function = lua.load("return function() local ok, message=pcall(require, module_a_value); getgenv().cycle_message=tostring(message); getgenv().cycle_result=not ok and string.find(tostring(message), 'circular ModuleScript require') ~= nil end").eval().unwrap();
    scheduler::enqueue(&lua, task, MultiValue::new()).unwrap();
    let scheduler_errors = run_scheduler(&lua);
    let cycle_result = lua
        .load("return getgenv().cycle_result")
        .eval::<Value>()
        .unwrap();
    let cycle_message = lua
        .load("return getgenv().cycle_message")
        .eval::<String>()
        .unwrap_or_default();
    let status = lua
        .load("local s=client.scriptStatus(); return s.modules_loaded, s.modules_loading, s.modules_failed, s.details[1] and s.details[1].error")
        .eval::<MultiValue>()
        .unwrap();
    assert!(
        matches!(cycle_result, Value::Boolean(true)),
        "cycle_message={cycle_message}, status={status:?}, scheduler_errors={scheduler_errors:?}"
    );
}

#[test]
fn source_admission_rejects_unverified_bytecode() {
    let snapshot = json!({"nodes": []});
    let (lua, _) = session(&snapshot);
    let candidate = json!({"script1":{"bytecode_hex":"0d030000"}});
    assert!(update_sources(&lua, &candidate).is_err());
}

#[test]
fn reconcile_schedules_only_ready_local_scripts_and_reports_completion() {
    let snapshot = json!({
        "initial_replication_complete":true,
        "nodes":[
            {"id":"root","class_name":"DataModel","name":"game","server_owned":true},
            {"id":"rf","class_name":"ReplicatedFirst","name":"ReplicatedFirst","parent":"root","server_owned":true},
            {"id":"ls","class_name":"LocalScript","name":"test","parent":"rf","server_owned":true}
        ],
        "script_sources":{"ls":{"source":"getgenv().ran_local_script=true"}}
    });
    let (lua, world) = session(&snapshot);
    assert_eq!(reconcile(&lua, &world).unwrap()["running"], 1);
    let scheduler_ref = lua
        .app_data_ref::<scheduler::SchedulerRef>()
        .unwrap()
        .clone();
    scheduler::step(
        &lua,
        &scheduler_ref,
        std::time::Instant::now(),
        std::time::Duration::from_millis(20),
    );
    assert_eq!(
        lua.load("return getgenv().ran_local_script")
            .eval::<bool>()
            .unwrap(),
        true
    );
    assert_eq!(reconcile(&lua, &world).unwrap()["completed"], 1);
}

#[test]
fn actor_scope_rejects_scripts_from_other_actor_instances() {
    let snapshot = json!({
        "nodes":[
            {"id":"root","class_name":"DataModel","name":"game","server_owned":true},
            {"id":"actor1","class_name":"Actor","name":"A","parent":"root","server_owned":true},
            {"id":"actor2","class_name":"Actor","name":"B","parent":"root","server_owned":true},
            {"id":"script2","class_name":"LocalScript","name":"Client","parent":"actor2","server_owned":true}
        ],
        "script_sources":{"script2":{"source":"getgenv().wrong_actor=true"}}
    });
    let (lua, world) = session(&snapshot);
    set_actor_id(&lua, Some("actor1".into())).unwrap();
    let status = reconcile(&lua, &world).unwrap();
    assert_eq!(status["running"], 0);
    assert_eq!(
        lua.load("return getgenv().wrong_actor")
            .eval::<Value>()
            .unwrap(),
        Value::Nil
    );
}
