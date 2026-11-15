#![cfg(feature = "rust-host")]
use headless_luau::runtime_owner::Runtime;
use serde_json::{json, Value};
use std::time::{Duration, Instant};

fn snapshot() -> Value {
    json!({"version":1,"sequence":0,"services":{"Workspace":"workspace"},"nodes":[
        {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{}},
        {"id":"workspace","class_name":"Workspace","name":"Workspace","parent":"game","properties":{}}
    ]})
}
fn execute(mut runtime: Runtime, source: &str) -> Value {
    let reply = runtime
        .call(json!({"op":"execute","source":source,"timeout_ms":2000}))
        .unwrap();
    assert_ne!(reply["ok"], false, "{reply}");
    let job = reply["job_id"].as_u64().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let reply = runtime.status(job).unwrap();
        assert_ne!(reply["ok"], false, "{reply}");
        if reply["completed"] == true {
            return reply;
        }
        assert!(
            Instant::now() < deadline,
            "curve script did not finish: {reply}"
        );
        let poll = runtime.poll().unwrap();
        assert!(
            poll["task_errors"].as_array().is_none_or(Vec::is_empty),
            "{poll}"
        );
        if let Some(actors) = poll["actor_runtime"].as_array() {
            for actor in actors {
                assert_ne!(actor["ok"], false, "{actor}");
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}
#[test]
fn packaged_curve_checks_run_in_the_official_vm() {
    let runtime = Runtime::new(snapshot(), 32).unwrap();
    let reply = execute(
        runtime,
        include_str!("../../examples/animation-curves-smoke.luau"),
    );
    assert_eq!(reply["values"][0]["passed"].as_f64(), Some(26.));
}
#[test]
fn actor_script_publishes_edited_and_cloned_curves_to_root_scripts() {
    let mut snapshot = snapshot();
    snapshot["nodes"].as_array_mut().unwrap().extend([
        json!({"id":"actor","class_name":"Actor","name":"Worker","parent":"workspace","properties":{},"attributes":{},"attributes_known":true}),
        json!({"id":"source","class_name":"LocalScript","name":"Source","parent":"actor","properties":{"Enabled":true,"Disabled":false}})
    ]);
    snapshot["script_sources"] = json!({"source":{"source":r#"
        local v=Instance.new('Vector3Curve',workspace);v.Name='FromActor'
        v:X():SetKeys({FloatCurveKey.new(0,0,Enum.KeyInterpolationMode.Linear),FloatCurveKey.new(2,4)})
        local copy=v:Clone();copy.Name='Copy';copy.Parent=workspace
        v:X():InsertKey(FloatCurveKey.new(2,8))
        script.Parent:SetAttribute('Ready',true)
    "#}});
    let runtime = Runtime::new(snapshot, 32).unwrap();
    let reply = execute(
        runtime,
        r#"
        local deadline=os.clock()+5
        while not workspace.Worker:GetAttribute('Ready') and os.clock()<deadline do task.wait() end
        assert(workspace.Worker:GetAttribute('Ready'))
        assert(workspace.FromActor:X():GetValueAtTime(1)==4)
        assert(workspace.Copy:X():GetValueAtTime(1)==2)
        return true
    "#,
    );
    assert_eq!(reply["values"][0], true);
}
