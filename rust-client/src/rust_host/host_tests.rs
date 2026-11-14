//! Focused tests for the standalone local Luau task scheduler.

use super::scheduler::{self, Scheduler, SchedulerRef, TaskOutcome, TaskPolicy, TaskStateKind};
use mlua::{IntoLua, Lua, MultiValue};
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

fn runtime() -> (Lua, SchedulerRef) {
    let lua = Lua::new();
    let scheduler = Rc::new(RefCell::new(Scheduler::new()));
    scheduler::install(&lua, &scheduler).unwrap();
    (lua, scheduler)
}

#[test]
fn spawned_tasks_can_reenter_scheduler_without_borrowing_it_across_lua() {
    let (lua, scheduler) = runtime();
    lua.load(
        r#"
        result = {}
        task.spawn(function(x, y)
            table.insert(result, x + y)
            child_thread = task.spawn(function(a, b) table.insert(result, a * b) end, 3, 4)
        end, 5, 6)
    "#,
    )
    .exec()
    .unwrap();

    let report = scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(50));
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    let values: Vec<i64> = lua
        .globals()
        .get::<mlua::Table>("result")
        .unwrap()
        .sequence_values()
        .map(Result::unwrap)
        .collect();
    assert_eq!(values, vec![11, 12]);
    let child: mlua::Thread = lua.globals().get("child_thread").unwrap();
    assert!(
        scheduler::take_outcome(&scheduler, &child).is_none(),
        "untracked callbacks should not retain result tuples"
    );
}

#[test]
fn tracked_job_keeps_its_complete_result_tuple_and_error() {
    let (lua, scheduler) = runtime();
    let policy =
        TaskPolicy::new(Duration::from_millis(5), Duration::from_millis(100), true).unwrap();
    let returns: mlua::Function = lua
        .load("return function() return 'ok', nil, 17 end")
        .eval()
        .unwrap();
    let thread = scheduler::enqueue_with_policy(&lua, returns, MultiValue::new(), policy).unwrap();
    scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(20));
    match scheduler::take_outcome(&scheduler, &thread).unwrap() {
        scheduler::TaskOutcome::Completed(values) => {
            assert_eq!(values.len(), 3);
            assert_eq!(
                values
                    .front()
                    .unwrap()
                    .as_string()
                    .unwrap()
                    .to_str()
                    .unwrap(),
                "ok"
            );
            assert!(matches!(values.get(1), Some(mlua::Value::Nil)));
            assert_eq!(values.get(2).unwrap().as_i64(), Some(17));
        }
        other => panic!("unexpected task result: {other:?}"),
    }

    let fails: mlua::Function = lua
        .load("return function() error('budget is just user text') end")
        .eval()
        .unwrap();
    let thread = scheduler::enqueue_with_policy(&lua, fails, MultiValue::new(), policy).unwrap();
    scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(20));
    assert!(
        matches!(scheduler::take_outcome(&scheduler, &thread), Some(scheduler::TaskOutcome::Error(message)) if message.contains("budget is just user text"))
    );

    let infinite: mlua::Function = lua
        .load("return function() while true do end end")
        .eval()
        .unwrap();
    let short_policy =
        TaskPolicy::new(Duration::from_millis(1), Duration::from_millis(1), false).unwrap();
    let thread =
        scheduler::enqueue_with_policy(&lua, infinite, MultiValue::new(), short_policy).unwrap();
    scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(20));
    assert!(matches!(
        scheduler::take_outcome(&scheduler, &thread),
        Some(scheduler::TaskOutcome::TimedOut(_))
    ));
}

#[test]
fn caught_non_yieldable_callback_timeout_is_terminal_and_does_not_poison_next_job() {
    let (lua, scheduler) = runtime();
    let policy =
        TaskPolicy::new(Duration::from_millis(2), Duration::from_millis(100), true).unwrap();
    let function = lua
        .load("pcall(function() table.sort({2,1}, function() while true do end end) end); return 'caught'")
        .into_function()
        .unwrap();
    let thread = scheduler::enqueue_with_policy(&lua, function, MultiValue::new(), policy).unwrap();
    scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(50));
    assert!(matches!(
        scheduler::take_outcome(&scheduler, &thread),
        Some(scheduler::TaskOutcome::TimedOut(message)) if message.contains("non-yieldable callback")
    ));
    assert_eq!(scheduler::snapshot(&scheduler).pending_tasks, 0);

    let next = lua.load("return 42").into_function().unwrap();
    let thread = scheduler::enqueue_with_policy(&lua, next, MultiValue::new(), policy).unwrap();
    scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(50));
    assert!(matches!(
        scheduler::take_outcome(&scheduler, &thread),
        Some(scheduler::TaskOutcome::Completed(values)) if values.front().and_then(mlua::Value::as_i64) == Some(42)
    ));
}

#[test]
fn task_wait_resumes_with_elapsed_time_on_a_later_step() {
    let (lua, scheduler) = runtime();
    lua.load(
        r#"
        waited = false
        task.spawn(function()
            local elapsed = task.wait(0.02)
            waited = elapsed >= 0.02
        end)
    "#,
    )
    .exec()
    .unwrap();
    scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(20));
    assert!(!lua.globals().get::<bool>("waited").unwrap());
    std::thread::sleep(Duration::from_millis(25));
    let report = scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(20));
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(lua.globals().get::<bool>("waited").unwrap());
}

#[test]
fn task_wait_accepts_luau_integer_zero_as_a_duration() {
    let (lua, scheduler) = runtime();
    let function: mlua::Function = lua
        .load("return function() local elapsed = task.wait(0); return elapsed >= 0 end")
        .eval()
        .unwrap();
    let policy =
        TaskPolicy::new(Duration::from_millis(5), Duration::from_millis(100), true).unwrap();
    let thread = scheduler::enqueue_with_policy(&lua, function, MultiValue::new(), policy).unwrap();
    scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(10));
    assert!(
        scheduler::diagnostics(&scheduler, &thread).unwrap().state == Some(TaskStateKind::Sleeping)
    );
    scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(10));
    assert!(
        matches!(scheduler::take_outcome(&scheduler, &thread), Some(scheduler::TaskOutcome::Completed(values)) if values.front().and_then(mlua::Value::as_boolean) == Some(true))
    );
}

#[test]
fn delay_keeps_callback_arguments_and_prepends_elapsed_time() {
    let (lua, scheduler) = runtime();
    lua.load(
        r#"
        delayed = false
        task.delay(0.01, function(elapsed, value)
            delayed = elapsed >= 0.01 and value == 73
        end, 73)
    "#,
    )
    .exec()
    .unwrap();
    scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(10));
    std::thread::sleep(Duration::from_millis(15));
    scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(10));
    assert!(lua.globals().get::<bool>("delayed").unwrap());
}

#[test]
fn cancellation_prevents_queued_task_from_running() {
    let (lua, scheduler) = runtime();
    lua.load(
        r#"
        cancelled_ran = false
        local thread = task.spawn(function() cancelled_ran = true end)
        task.cancel(thread)
    "#,
    )
    .exec()
    .unwrap();
    scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(10));
    assert!(!lua.globals().get::<bool>("cancelled_ran").unwrap());
}

#[test]
fn queue_snapshot_reports_real_ready_sleeping_waiting_and_limit_counts() {
    let (lua, scheduler) = runtime();
    lua.load(
        r#"
        task.spawn(function() task.wait(10) end)
        task.spawn(function() coroutine.yield('host event') end)
    "#,
    )
    .exec()
    .unwrap();
    scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(20));
    let snapshot = scheduler::snapshot(&scheduler);
    assert_eq!(snapshot.active_tasks, 2);
    assert_eq!(snapshot.pending_tasks, 2);
    assert_eq!(snapshot.ready, 0);
    assert_eq!(snapshot.sleeping, 1);
    assert_eq!(snapshot.waiting, 1);
    assert_eq!(snapshot.running, 0);
    assert_eq!(snapshot.queue_limit, 2048);
    assert!(!snapshot.ready_owner_attribution_available);
}

#[test]
fn callback_resume_preserves_the_full_argument_tuple() {
    let (lua, scheduler) = runtime();
    let function: mlua::Function = lua
        .load("return function(...) callback_values = {...} end")
        .eval()
        .unwrap();
    let thread = lua.create_thread(function).unwrap();
    scheduler::resume(
        &lua,
        thread.clone(),
        MultiValue::from_vec(vec![
            1_i64.into_lua(&lua).unwrap(),
            "two".into_lua(&lua).unwrap(),
        ]),
    )
    .unwrap();
    scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(10));
    let values: Vec<mlua::Value> = lua
        .globals()
        .get::<mlua::Table>("callback_values")
        .unwrap()
        .sequence_values()
        .map(Result::unwrap)
        .collect();
    assert_eq!(values[0].as_i64(), Some(1));
    assert_eq!(values[1].as_string().unwrap().to_str().unwrap(), "two");
}

#[test]
fn local_thread_identity_defaults_inherits_isolates_and_prints_to_console() {
    let (lua, scheduler) = runtime();
    let world = Rc::new(RefCell::new(
        super::world::World::from_snapshot(
            &serde_json::json!({"version":1,"sequence":0,"nodes":[]}),
            None,
        )
        .unwrap(),
    ));
    super::source_helpers::install(&lua, &world).unwrap();
    let function = lua.load(r#"
        local root = getthreadidentity()
        setthreadidentity(6)
        task.spawn(function()
            child_identity = getidentity()
            setidentity(4)
            task.spawn(function() grandchild_identity = getthreadcontext() end)
            local co = coroutine.create(function() direct_identity = getthreadidentity(); setthreadidentity(3) end)
            assert(coroutine.resume(co))
            printidentity()
        end)
        return root
    "#).into_function().unwrap();
    let policy =
        TaskPolicy::new(Duration::from_millis(5), Duration::from_millis(100), true).unwrap();
    let root = scheduler::enqueue_with_policy(&lua, function, MultiValue::new(), policy).unwrap();
    scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(30));
    assert!(
        matches!(scheduler::take_outcome(&scheduler, &root), Some(scheduler::TaskOutcome::Completed(values)) if values.front().and_then(mlua::Value::as_i64) == Some(8))
    );
    assert_eq!(lua.globals().get::<i64>("child_identity").unwrap(), 6);
    assert_eq!(lua.globals().get::<i64>("grandchild_identity").unwrap(), 4);
    assert_eq!(lua.globals().get::<i64>("direct_identity").unwrap(), 4);
    let output = super::source_helpers::drain_console(&lua);
    assert!(
        output
            .iter()
            .any(|(level, text)| level == "info" && text == "Current identity is 4"),
        "{output:?}"
    );

    let game_script = lua
        .load("game_script_identity = getthreadidentity()")
        .into_function()
        .unwrap();
    let game_thread = scheduler::enqueue_owned(
        &lua,
        game_script,
        MultiValue::new(),
        "script:local-test".into(),
    )
    .unwrap();
    scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(10));
    assert_eq!(lua.globals().get::<i64>("game_script_identity").unwrap(), 2);
    let _ = game_thread;
}

#[test]
fn callback_keeps_creator_identity_after_creator_task_finishes() {
    let (lua, scheduler) = runtime();
    let function = lua.load("setthreadidentity(5)").into_function().unwrap();
    let policy =
        TaskPolicy::new(Duration::from_millis(5), Duration::from_millis(100), true).unwrap();
    let creator =
        scheduler::enqueue_with_policy(&lua, function, MultiValue::new(), policy).unwrap();
    scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(10));
    assert!(matches!(
        scheduler::take_outcome(&scheduler, &creator),
        Some(scheduler::TaskOutcome::Completed(_))
    ));

    let callback = lua
        .load("return function() callback_identity = getthreadidentity() end")
        .eval::<mlua::Function>()
        .unwrap();
    let callback_thread =
        scheduler::enqueue_from(&lua, callback, MultiValue::new(), Some(&creator)).unwrap();
    scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(10));
    assert_eq!(lua.globals().get::<i64>("callback_identity").unwrap(), 5);
    assert!(matches!(
        scheduler::take_outcome(&scheduler, &callback_thread),
        Some(scheduler::TaskOutcome::Yielded(_)) | None
    ));
}

#[test]
fn nested_coroutine_preemption_resumes_transparently_and_preserves_results() {
    let (lua, scheduler) = runtime();
    let policy =
        TaskPolicy::new(Duration::from_millis(1), Duration::from_millis(500), true).unwrap();
    for source in [
        r#"local leaf=coroutine.create(function() local n=0; for i=1,6000000 do n+=i end; return 'resume-ok',nil,n end); local middle=coroutine.create(function() return coroutine.resume(leaf) end); return coroutine.resume(middle)"#,
        r#"local child=coroutine.wrap(function() local n=0; for i=1,6000000 do n+=i end; return 'wrap-ok',nil,n end); return child()"#,
    ] {
        let function = lua.load(source).into_function().unwrap();
        let thread =
            scheduler::enqueue_with_policy(&lua, function, MultiValue::new(), policy).unwrap();
        for _ in 0..600 {
            scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(5));
            if scheduler::diagnostics(&scheduler, &thread).is_some_and(|d| {
                matches!(
                    d.state,
                    Some(
                        TaskStateKind::Completed | TaskStateKind::Error | TaskStateKind::Cancelled
                    )
                )
            }) {
                break;
            }
        }
        let outcome = scheduler::take_outcome(&scheduler, &thread)
            .expect("nested coroutine job should finish");
        let TaskOutcome::Completed(values) = outcome else {
            panic!("nested coroutine did not complete: {outcome:?}")
        };
        let expected = if source.contains("resume-ok") {
            "resume-ok"
        } else {
            "wrap-ok"
        };
        let result_offset = if expected == "resume-ok" { 2 } else { 0 };
        assert_eq!(
            values.len(),
            result_offset + 3,
            "preserves nested result tuple and nil"
        );
        if result_offset == 2 {
            assert_eq!(values.get(0).and_then(mlua::Value::as_boolean), Some(true));
            assert_eq!(values.get(1).and_then(mlua::Value::as_boolean), Some(true));
        }
        assert_eq!(
            values
                .get(result_offset)
                .and_then(|v| v.as_string())
                .unwrap()
                .to_str()
                .unwrap(),
            expected
        );
        assert!(matches!(
            values.get(result_offset + 1),
            Some(mlua::Value::Nil)
        ));
        assert_eq!(
            values.get(result_offset + 2).and_then(mlua::Value::as_i64),
            Some(18_000_003_000_000)
        );
        assert!(
            scheduler::diagnostics(&scheduler, &thread)
                .unwrap()
                .slice_interruptions
                > 0
        );
    }
}

#[test]
fn infinite_nested_coroutine_times_out_without_becoming_success() {
    let (lua, scheduler) = runtime();
    let function = lua.load(r#"local co=coroutine.create(function() local n=0; while true do n+=1 end end); return coroutine.resume(co)"#).into_function().unwrap();
    let policy =
        TaskPolicy::new(Duration::from_millis(1), Duration::from_millis(30), true).unwrap();
    let thread = scheduler::enqueue_with_policy(&lua, function, MultiValue::new(), policy).unwrap();
    for _ in 0..100 {
        scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(5));
        if scheduler::diagnostics(&scheduler, &thread).is_some_and(|d| {
            matches!(
                d.state,
                Some(TaskStateKind::Completed | TaskStateKind::Error | TaskStateKind::Cancelled)
            )
        }) {
            break;
        }
    }
    assert!(matches!(
        scheduler::take_outcome(&scheduler, &thread),
        Some(TaskOutcome::TimedOut(_))
    ));
}

#[test]
fn finite_non_yieldable_callback_may_cross_soft_slice_but_not_hard_budget() {
    let (lua, scheduler) = runtime();
    lua.globals()
        .set(
            "soft_callback",
            lua.create_function(|_, ()| {
                std::thread::sleep(Duration::from_millis(15));
                Ok(())
            })
            .unwrap(),
        )
        .unwrap();
    let function = lua
        .load(
            r#"local values={2,1}
                table.sort(values, function(a,b)
                    soft_callback()
                    local inside=0; for i=1,50000 do inside+=i end
                    return a<b and inside>0
                end)
                local after=0; for i=1,50000 do after+=i end
                return #values, after"#,
        )
        .into_function()
        .unwrap();
    let policy =
        TaskPolicy::new(Duration::from_millis(1), Duration::from_millis(500), true).unwrap();
    let thread = scheduler::enqueue_with_policy(&lua, function, MultiValue::new(), policy).unwrap();
    for _ in 0..200 {
        scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(5));
        if scheduler::diagnostics(&scheduler, &thread).is_some_and(|d| {
            matches!(
                d.state,
                Some(TaskStateKind::Completed | TaskStateKind::Error | TaskStateKind::Cancelled)
            )
        }) {
            break;
        }
    }
    let outcome = scheduler::take_outcome(&scheduler, &thread);
    let values = match outcome {
        Some(TaskOutcome::Completed(values)) => values,
        other => panic!(
            "finite non-yieldable callback should complete before the hard deadline; diagnostics={:?}, outcome={other:?}",
            scheduler::diagnostics(&scheduler, &thread)
        ),
    };
    let observed = values.front().and_then(|value| {
        value
            .as_f64()
            .or_else(|| value.as_i64().map(|number| number as f64))
    });
    assert_eq!(observed, Some(2.0), "callback result tuple: {values:?}");
    assert_eq!(
        values
            .get(1)
            .and_then(|value| value.as_f64().or_else(|| value.as_i64().map(|n| n as f64))),
        Some(1_250_025_000.0),
        "post-callback Luau work completed: {values:?}"
    );
    assert!(
        scheduler::diagnostics(&scheduler, &thread)
            .unwrap()
            .slice_interruptions
            > 0
    );
}

#[test]
fn plain_coroutine_yield_parks_and_round_trips_both_tuples() {
    let (lua, scheduler) = runtime();
    let function: mlua::Function = lua
        .load(
            r#"return function()
            local a, b, c = coroutine.yield({}, nil, 4)
            resumed_values = {a, b, c, n = select('#', a, b, c)}
        end"#,
        )
        .eval()
        .unwrap();
    let policy =
        TaskPolicy::new(Duration::from_millis(5), Duration::from_millis(250), true).unwrap();
    let thread = scheduler::enqueue_with_policy(&lua, function, MultiValue::new(), policy).unwrap();

    let first = scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(20));
    assert_eq!(first.ready, 0);
    assert!(scheduler::is_parked(&scheduler, &thread));
    assert_eq!(
        scheduler::diagnostics(&scheduler, &thread).unwrap().state,
        Some(TaskStateKind::Waiting)
    );
    let values = scheduler::take_yielded(&scheduler, &thread).unwrap();
    assert_eq!(values.len(), 3);
    assert!(matches!(values.front(), Some(mlua::Value::Table(_))));
    assert!(matches!(values.get(1), Some(mlua::Value::Nil)));
    assert_eq!(values.get(2).unwrap().as_i64(), Some(4));
    scheduler::resume(
        &lua,
        thread.clone(),
        MultiValue::from_vec(vec![
            9_i64.into_lua(&lua).unwrap(),
            mlua::Value::Nil,
            8_i64.into_lua(&lua).unwrap(),
        ]),
    )
    .unwrap();
    scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(20));
    let resumed: mlua::Table = lua.globals().get("resumed_values").unwrap();
    assert_eq!(resumed.get::<i64>(1).unwrap(), 9);
    assert_eq!(resumed.get::<mlua::Value>(2).unwrap(), mlua::Value::Nil);
    assert_eq!(resumed.get::<i64>(3).unwrap(), 8);
    assert_eq!(resumed.get::<i64>("n").unwrap(), 3);
    assert!(matches!(
        scheduler::take_outcome(&scheduler, &thread),
        Some(scheduler::TaskOutcome::Completed(_))
    ));
    let diagnostics = scheduler::diagnostics(&scheduler, &thread).unwrap();
    assert_eq!(diagnostics.state, Some(TaskStateKind::Completed));
    assert_eq!(diagnostics.resumes, 2);
    assert_eq!(diagnostics.yields, 1);
}
