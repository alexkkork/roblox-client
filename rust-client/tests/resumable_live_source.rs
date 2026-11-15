//! The live Rust client may opt its authored root job into frame-to-frame
//! CPU slices. Standalone jobs keep their single-request timeout by default.
use headless_luau::runtime_owner::{successful, Runtime};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

fn runtime() -> Runtime {
    let snapshot: Value = serde_json::from_str(include_str!(
        "../../tools/fixtures/rust-rewrite/compact/snapshot.json"
    ))
    .unwrap();
    Runtime::new(snapshot, 32).unwrap()
}

fn completed(response: &Value, job_id: u64) -> Option<Value> {
    response["completed_jobs"]
        .as_array()?
        .iter()
        .find(|job| job["job_id"].as_u64() == Some(job_id))
        .cloned()
}

fn record_interruptions(response: &Value, job_id: u64, total: &mut u64) {
    if let Some(rows) = response["job_diagnostics"].as_array() {
        for row in rows {
            if row["job_id"].as_u64() == Some(job_id) {
                *total = (*total).max(row["slice_interruptions"].as_u64().unwrap_or(0));
            }
        }
    }
}

#[test]
fn opted_in_finite_authored_work_resumes_across_five_ms_polls() {
    let mut runtime = runtime();
    let started = Instant::now();
    let first = runtime
        .call(json!({
            "op":"execute",
            "timeout_ms":1,
            "resume_across_frames":true,
            "source":"local sum=0; for i=1,10000000 do sum+=i end; return sum"
        }))
        .unwrap();
    successful(&first).unwrap();
    let job_id = first["job_id"]
        .as_u64()
        .expect("execute must return the authored job id");
    let mut interruptions = 0;
    record_interruptions(&first, job_id, &mut interruptions);
    assert_eq!(
        first["state"], "running",
        "work should span more than its 1 ms request slice: {first}"
    );

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut result = completed(&first, job_id);
    let mut response = first;
    while result.is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
        response = runtime.call(json!({"op":"poll","timeout_ms":5})).unwrap();
        successful(&response).unwrap();
        record_interruptions(&response, job_id, &mut interruptions);
        result = completed(&response, job_id);
    }
    let result =
        result.unwrap_or_else(|| panic!("finite job did not finish; last response={response}"));
    let expected = 10_000_000_f64 * 10_000_001_f64 / 2.0;
    assert_eq!(
        result["ok"], true,
        "resumed job should complete successfully: {result}"
    );
    assert_eq!(result["values"][0].as_f64(), Some(expected));
    assert!(
        interruptions > 1,
        "expected multiple VM slices, got {interruptions}"
    );
    eprintln!(
        "finite authored job completed over {interruptions} recorded slice interruptions in {:.2} ms",
        started.elapsed().as_secs_f64() * 1000.0
    );
}

#[test]
fn opted_in_non_yielding_loop_still_hits_the_existing_two_second_cap() {
    let mut runtime = runtime();
    let started = Instant::now();
    let first = runtime
        .call(json!({
            "op":"execute",
            "timeout_ms":1,
            "resume_across_frames":true,
            "source":"while true do end"
        }))
        .unwrap();
    successful(&first).unwrap();
    let job_id = first["job_id"]
        .as_u64()
        .expect("execute must return the authored job id");
    // This cap counts execution inside the VM, not time spent sleeping or
    // waiting for the host scheduler. Leave wall-clock headroom on busy CI
    // machines, and check the actual execution counter on every active poll.
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut result = completed(&first, job_id);
    let mut response = first;
    while result.is_none() && Instant::now() < deadline {
        std::thread::yield_now();
        response = runtime.call(json!({"op":"poll","timeout_ms":5})).unwrap();
        successful(&response).unwrap();
        result = completed(&response, job_id);
        for row in response["job_diagnostics"].as_array().unwrap() {
            if row["job_id"].as_u64() == Some(job_id) {
                let execution_ms = row["execution_ms"].as_f64().unwrap();
                assert!(
                    execution_ms <= 2000.0,
                    "non-yielding job remained active beyond its CPU cap: {row}"
                );
            }
        }
    }
    let elapsed = started.elapsed();
    let result = result.unwrap_or_else(|| {
        panic!("non-yielding loop escaped its cumulative CPU cap; last response={response}")
    });
    assert_eq!(result["ok"], false);
    assert_eq!(result["timed_out"], true);
    eprintln!(
        "non-yielding authored job stopped after {:.2} seconds",
        elapsed.as_secs_f64()
    );
}

#[test]
fn standalone_authored_jobs_keep_the_default_single_request_timeout() {
    let mut runtime = runtime();
    let response = runtime
        .call(json!({
            "op":"execute",
            "timeout_ms":1,
            "source":"while true do end"
        }))
        .unwrap();
    assert_eq!(response["ok"], false);
    assert_eq!(response["timed_out"], true);
    let job_id = response["job_id"]
        .as_u64()
        .expect("execute must return the authored job id");
    let result = runtime.status(job_id).unwrap();
    assert_eq!(result["ok"], false);
    assert_eq!(result["timed_out"], true);
}
