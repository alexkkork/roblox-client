use super::*;
use serde_json::Value as Json;
use std::time::{Duration, Instant};

fn run_json(source: &str, args: &[f64], timeout_ms: u32) -> Json {
    serde_json::from_slice(&run(source.as_bytes(), args, timeout_ms, 64)).unwrap()
}

#[test]
fn no_snapshot_run_has_value_constructors_but_no_synthetic_game_world() {
    let response = run_json(
        "return game == nil, workspace == nil, typeof(UDim.new(0.5, 4))",
        &[],
        250,
    );
    assert_eq!(response["ok"], true, "{response}");
    assert_eq!(response["values"], json!([true, true, "UDim"]));
    assert!(response.get("persistent").is_none());
    assert!(response.get("job_id").is_none());
    assert!(response.get("runtime_thread_integrity").is_none());
    assert_eq!(response["runtime_protected_integrity"]["constructions"], 1);
    assert_eq!(response["runtime_protected_integrity"]["copies"], 1);
}

#[test]
fn snapshot_run_installs_world_bindings_and_returns_rich_mutations() {
    let snapshot = json!({
        "version":1,
        "sequence":7,
        "services":{"Workspace":"workspace"},
        "nodes":[
            {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{}},
            {"id":"workspace","class_name":"Workspace","name":"Workspace","parent":"game","properties":{"Gravity":196.2}}
        ]
    });
    let response: Json = serde_json::from_slice(&run_with_snapshot(
        b"workspace.Gravity = 80; return workspace.Gravity, workspace",
        &[],
        &serde_json::to_vec(&snapshot).unwrap(),
        250,
        64,
    ))
    .unwrap();
    assert_eq!(response["ok"], true, "{response}");
    assert_eq!(response["values"][0], json!(80.0));
    assert_eq!(response["values"][1]["$ref"], json!("workspace"));
    let mutations = response["mutations"].as_array().unwrap();
    let gravity_writes: Vec<_> = mutations
        .iter()
        .filter(|mutation| mutation["kind"] == "set")
        .collect();
    assert_eq!(gravity_writes.len(), 1, "{mutations:?}");
    assert_eq!(gravity_writes[0]["id"], json!("workspace"));
    assert_eq!(gravity_writes[0]["property"], json!("Gravity"));
    assert_eq!(gravity_writes[0]["value"], json!(80.0));
    assert!(mutations
        .iter()
        .all(|mutation| mutation["kind"] == "create" || mutation["kind"] == "set"));
    assert_eq!(response["runtime_thread_integrity"]["checks"], json!(1));
    assert_eq!(
        response["runtime_protected_integrity"]["constructions"],
        json!(1)
    );
}

#[test]
fn snapshot_run_rejects_invalid_json_and_oversized_snapshots() {
    let invalid: Json = serde_json::from_slice(&run_with_snapshot(
        b"return true",
        &[],
        b"not json",
        100,
        64,
    ))
    .unwrap();
    assert_eq!(invalid["ok"], false);
    assert!(invalid["error"]
        .as_str()
        .unwrap()
        .contains("invalid snapshot"));

    let oversized = vec![b' '; MAX_SNAPSHOT_BYTES + 1];
    let invalid: Json =
        serde_json::from_slice(&run_with_snapshot(b"return true", &[], &oversized, 100, 64))
            .unwrap();
    assert_eq!(invalid["ok"], false);
    assert!(invalid["error"].as_str().unwrap().contains("exceeds 8 MiB"));
}

#[test]
fn standalone_wait_resumes_until_the_script_completes() {
    let response = run_json("task.wait(0.025); return 'woke'", &[], 250);
    assert_eq!(response["ok"], true, "{response}");
    assert_eq!(response["values"], json!(["woke"]));
}

#[test]
fn standalone_timeout_is_wall_bounded_and_reported_as_timeout() {
    let started = Instant::now();
    let response = run_json("while true do end", &[], 30);
    assert_eq!(response["ok"], false, "{response}");
    assert_eq!(response["timed_out"], true, "{response}");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn user_error_text_does_not_become_a_timeout() {
    let response = run_json("error('deadline budget')", &[], 100);
    assert_eq!(response["ok"], false, "{response}");
    assert_eq!(response["timed_out"], false, "{response}");
    assert!(response["error"]
        .as_str()
        .unwrap()
        .contains("deadline budget"));
}

#[test]
fn standalone_results_reject_nonprimitive_values_and_preserve_arguments() {
    let returned = run_json("return ...", &[1.25, 7.0], 100);
    assert_eq!(returned["ok"], true, "{returned}");
    assert_eq!(returned["values"], json!([1.25, 7]));

    let invalid = run_json("return {}", &[], 100);
    assert_eq!(invalid["ok"], false, "{invalid}");
    assert!(invalid["error"].as_str().unwrap().contains("primitive"));
}
