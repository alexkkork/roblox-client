#![cfg(feature = "rust-host")]

use headless_luau::runtime_owner::{successful, Runtime};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

#[test]
fn complete_engine_smoke_runs_through_the_persistent_runtime() {
    let snapshot: Value =
        serde_json::from_str(include_str!("../../tests/fixtures/world_snapshot.json")).unwrap();
    let mut runtime = Runtime::new(snapshot, 32).unwrap();
    let response = runtime
        .call(json!({
            "op":"execute",
            "source":include_str!("../../examples/rust-host-smoke.luau"),
            "timeout_ms":100,
        }))
        .unwrap();
    successful(&response).unwrap();
    let job = response["job_id"].as_u64().unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let result = runtime.status(job).unwrap();
        if result["completed"] == true {
            successful(&result).unwrap();
            assert_eq!(result["values"][0]["passed"], json!(12.0), "{result}");
            // Persistent output preserves the legacy numeric-key table shape.
            let checks = result["values"][0]["checks"].as_object().unwrap();
            let expected = [
                "received humanoid",
                "part defaults",
                "local mass",
                "mass after material change",
                "property reset",
                "instance selector",
                "instance handle",
                "settings singleton",
                "stats singleton",
                "engine types",
                "signal tuple",
                "typed values",
            ];
            assert_eq!(checks.len(), expected.len(), "{result}");
            for (index, name) in expected.into_iter().enumerate() {
                assert_eq!(checks[&format!("{:.6}", (index + 1) as f64)], json!(name));
            }
            break;
        }
        assert!(
            Instant::now() < deadline,
            "engine smoke did not complete: {result}"
        );
        successful(&runtime.poll().unwrap()).unwrap();
        std::thread::sleep(Duration::from_millis(1));
    }
}
