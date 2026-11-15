#![cfg(feature = "rust-host")]
use headless_luau::runtime_owner::Runtime;
use serde_json::{json, Value};
use std::time::{Duration, Instant};
fn check(reply: &Value) {
    assert_ne!(reply["ok"], false, "{reply}");
    for key in ["task_errors", "animation_pose_errors", "joint_pose_errors"] {
        assert!(
            reply[key].as_array().is_none_or(|v| v.is_empty()),
            "{reply}"
        );
    }
    if let Some(actors) = reply["actor_runtime"].as_array() {
        for actor in actors {
            check(actor);
            check(&actor["world_effects"]);
        }
    }
}
#[test]
fn packaged_animation_pose_smoke_uses_official_luau_and_real_frame_phases() {
    let mut runtime=Runtime::new(json!({"version":1,"sequence":0,"services":{"Workspace":"workspace"},"nodes":[
        {"id":"workspace","class_name":"Workspace","name":"Workspace","parent":null,"properties":{}}
    ]}),32).unwrap();
    let reply=runtime.call(json!({"op":"execute","source":include_str!("../../examples/animation-poses-smoke.luau"),"timeout_ms":2000})).unwrap();
    check(&reply);
    let job = reply["job_id"].as_u64().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let reply = runtime.status(job).unwrap();
        check(&reply);
        if reply["completed"] == true {
            assert_eq!(reply["values"][0]["passed"].as_f64(), Some(18.), "{reply}");
            break;
        }
        assert!(Instant::now() < deadline, "{reply}");
        check(&runtime.poll().unwrap());
        std::thread::sleep(Duration::from_millis(1));
    }
}
