//! Bounded, data-only execution path for the legacy `run()` entrypoint.
//!
//! Without a snapshot, this exposes value constructors and local task helpers.
//! With a snapshot, it installs the same bounded one-shot world surface as the
//! legacy entrypoint; persistent source and network helpers stay separate.

use super::{
    bindables, cframe, closures, enums, errors, geometry, input, instance_helpers, integrity,
    misc_values, movement, numeric, playback, remotes, scheduler, sequences, services,
    shared_tables, signals, temporal, value_types, values, world,
};
use mlua::{Lua, MultiValue, Value};
use serde_json::{json, Value as Json};
use std::{
    cell::RefCell,
    rc::Rc,
    thread,
    time::{Duration, Instant},
};

const MAX_SOURCE: usize = 100 * 1024 * 1024;
const MAX_RETURN_STRING_BYTES: usize = 256 * 1024;
const MAX_OUTPUT_BYTES: usize = 300 * 1024;
const MAX_SNAPSHOT_BYTES: usize = 8 * 1024 * 1024;

#[cfg(test)]
#[path = "standalone_tests.rs"]
mod tests;

enum RootResult {
    Running,
    Completed(MultiValue),
    Failed(String),
    TimedOut,
    Cancelled,
}

fn failure(error: impl Into<String>, timed_out: bool) -> Json {
    json!({"ok":false,"values":[],"error":error.into(),"timed_out":timed_out})
}

fn minimal_world() -> Result<world::WorldRef, String> {
    let snapshot = json!({"version":1,"sequence":0,"nodes":[],"services":{}});
    let world = world::World::from_snapshot(&snapshot, None)?;
    Ok(Rc::new(RefCell::new(world)))
}

fn install_values(lua: &Lua, ctx: &world::WorldRef) -> mlua::Result<()> {
    // These match the native authored value families while keeping the
    // installed WorldRef private to scalar constructors which need an owner.
    integrity::install(lua)?;
    enums::install(lua)?;
    numeric::install(lua)?;
    cframe::install(lua)?;
    value_types::install(lua)?;
    sequences::install(lua)?;
    temporal::install(lua)?;
    misc_values::install(lua, ctx)?;
    closures::install(lua)?;

    // Keep the safe base library but remove process, filesystem, dynamic-load,
    // debug, and stdout access just like the standalone native host.
    let globals = lua.globals();
    for name in ["require", "loadstring", "dofile", "loadfile", "io", "print"] {
        globals.set(name, Value::Nil)?;
    }
    let full_debug: mlua::Table = globals.get("debug")?;
    let safe_debug = lua.create_table()?;
    for name in ["info", "traceback"] {
        if let Ok(value) = full_debug.get::<Value>(name) {
            safe_debug.set(name, value)?;
        }
    }
    globals.set("debug", safe_debug)?;
    let os: mlua::Table = globals.get("os")?;
    os.set("exit", Value::Nil)?;
    Ok(())
}

/// Install the engine surface used by the legacy one-shot world-backed path.
/// Source executor helpers, host I/O, and lifecycle autostart are intentionally
/// omitted: native `run_vm(..., source_bindings = None)` does not install them.
fn install_world(lua: &Lua, ctx: &world::WorldRef) -> mlua::Result<()> {
    errors::install(lua)?;
    signals::install(lua, ctx)?;
    world::install(lua, ctx)?;
    bindables::install(lua, ctx)?;
    geometry::install(lua, ctx)?;
    movement::install(lua, ctx)?;
    input::install(lua, ctx)?;
    super::gui::install(lua, ctx)?;
    playback::install(lua, ctx)?;
    services::install(lua, ctx)?;
    remotes::install(lua, ctx)?;
    instance_helpers::install(lua, ctx)?;
    shared_tables::install(lua, ctx, shared_tables::SharedTableRegistry::default())?;
    Ok(())
}

fn capture_values(values: MultiValue) -> mlua::Result<Vec<Json>> {
    let mut result = Vec::with_capacity(values.len());
    let mut string_bytes = 0usize;
    for value in values {
        if let Value::String(text) = &value {
            string_bytes = string_bytes.saturating_add(text.as_bytes().len());
            if string_bytes > MAX_RETURN_STRING_BYTES {
                return Err(mlua::Error::RuntimeError(
                    "return strings exceed 256 KiB".into(),
                ));
            }
        }
        result.push(values::strict_result_json(&value)?);
    }
    Ok(result)
}

fn add_integrity(lua: &Lua, response: &mut Json, has_world: bool) {
    if let Ok((threads, protected)) = integrity::report(lua) {
        // The legacy standalone host has no DataModel owner, so it reports
        // protected-source observations but deliberately omits thread
        // integrity, which belongs to a world-backed session.
        if has_world {
            response["runtime_thread_integrity"] = threads;
        }
        response["runtime_protected_integrity"] = protected;
    }
}

/// Run source without a world snapshot and return the same primitive-only
/// result envelope as the standalone native host.
pub fn run(source: &[u8], arguments: &[f64], timeout_ms: u32, memory_mb: u32) -> Vec<u8> {
    run_inner(source, arguments, None, timeout_ms, memory_mb)
}

/// Run source against a supplied world snapshot using the legacy one-shot
/// engine bindings and rich mutation/result serialization.
pub fn run_with_snapshot(
    source: &[u8],
    arguments: &[f64],
    snapshot: &[u8],
    timeout_ms: u32,
    memory_mb: u32,
) -> Vec<u8> {
    run_inner(source, arguments, Some(snapshot), timeout_ms, memory_mb)
}

fn run_inner(
    source: &[u8],
    arguments: &[f64],
    snapshot_bytes: Option<&[u8]>,
    timeout_ms: u32,
    memory_mb: u32,
) -> Vec<u8> {
    if source.len() > MAX_SOURCE {
        return encode(failure("input exceeds 100 MiB limit", false));
    }
    if !(1..=2000).contains(&timeout_ms) || !(8..=512).contains(&memory_mb) {
        return encode(failure("invalid source or resource limit", false));
    }
    if arguments.len() > 64 || arguments.iter().any(|v| !v.is_finite()) {
        return encode(failure("arguments after -- must be finite numbers", false));
    }
    if snapshot_bytes.is_some_and(|bytes| bytes.len() > MAX_SNAPSHOT_BYTES) {
        return encode(failure("world snapshot exceeds 8 MiB", false));
    }
    let source = match std::str::from_utf8(source) {
        Ok(source) => source,
        Err(_) => return encode(failure("source must be UTF-8", false)),
    };
    let snapshot = match snapshot_bytes {
        Some(bytes) => match serde_json::from_slice::<Json>(bytes) {
            Ok(snapshot) => Some(snapshot),
            Err(error) => return encode(failure(format!("invalid snapshot: {error}"), false)),
        },
        None => None,
    };
    if let Err(error) = rove_runtime_sys::verify_abi() {
        return encode(failure(error.to_string(), false));
    }

    // The wall deadline includes VM setup, compilation, execution, timers, and
    // background work, matching SourceScheduler::run.
    let deadline = Instant::now() + Duration::from_millis(u64::from(timeout_ms));
    let lua = Lua::new();
    if let Err(error) = lua.set_memory_limit(memory_mb as usize * 1024 * 1024) {
        return encode(failure(error.to_string(), false));
    }
    // Hold the WorldRef for the full VM lifetime because engine userdata keep
    // Weak references to it. The private empty world is only used to anchor
    // value constructors in no-snapshot mode; it is never installed globally.
    let ctx = match snapshot.as_ref() {
        Some(snapshot) => match world::World::from_snapshot(snapshot, None) {
            Ok(world) => Rc::new(RefCell::new(world)),
            Err(error) => return encode(failure(format!("invalid snapshot: {error}"), false)),
        },
        None => match minimal_world() {
            Ok(world) => world,
            Err(error) => return encode(failure(error, false)),
        },
    };
    if let Err(error) = install_values(&lua, &ctx) {
        let mut response = failure(error.to_string(), false);
        add_integrity(&lua, &mut response, snapshot.is_some());
        return encode(response);
    }
    if snapshot.is_some() {
        if let Err(error) = install_world(&lua, &ctx) {
            let mut response = failure(error.to_string(), false);
            add_integrity(&lua, &mut response, snapshot.is_some());
            return encode(response);
        }
    }

    let scheduler_ref = Rc::new(RefCell::new(scheduler::Scheduler::new()));
    if let Err(error) = scheduler::install(&lua, &scheduler_ref) {
        let mut response = failure(error.to_string(), false);
        add_integrity(&lua, &mut response, snapshot.is_some());
        return encode(response);
    }
    if let Err(error) = lua.sandbox(true) {
        let mut response = failure(error.to_string(), false);
        add_integrity(&lua, &mut response, snapshot.is_some());
        return encode(response);
    }
    if snapshot.is_some() {
        // Engine properties are mutable. Import caching would otherwise keep
        // a value read during load after its Instance setter changes it.
        lua.globals().set_safeenv(false);
    }
    let function = match integrity::load_source(&lua, "=user-source", source) {
        Ok(function) => function,
        Err(error) => {
            let mut response = failure(error.to_string(), false);
            add_integrity(&lua, &mut response, snapshot.is_some());
            return encode(response);
        }
    };
    let args = MultiValue::from_vec(arguments.iter().copied().map(Value::Number).collect());
    let policy = match scheduler::TaskPolicy::new(
        Duration::from_millis(u64::from(timeout_ms)),
        Duration::from_millis(u64::from(timeout_ms)),
        false,
    ) {
        Ok(policy) => policy,
        Err(error) => {
            let mut response = failure(error.to_string(), false);
            add_integrity(&lua, &mut response, snapshot.is_some());
            return encode(response);
        }
    };
    let root = match scheduler::enqueue_with_policy(&lua, function, args, policy) {
        Ok(thread) => thread,
        Err(error) => {
            let mut response = failure(error.to_string(), false);
            add_integrity(&lua, &mut response, snapshot.is_some());
            return encode(response);
        }
    };

    let mut root_result = RootResult::Running;
    let mut task_errors: Vec<String> = Vec::new();
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let report = scheduler::step(
            &lua,
            &scheduler_ref,
            now,
            deadline.saturating_duration_since(now),
        );
        for error in report.errors {
            if task_errors.len() == 64 {
                task_errors.remove(0);
            }
            task_errors.push(error);
        }
        if let Some(outcome) = scheduler::take_outcome(&scheduler_ref, &root) {
            match outcome {
                scheduler::TaskOutcome::Completed(values) => {
                    root_result = RootResult::Completed(values);
                }
                scheduler::TaskOutcome::Yielded(_) => {}
                scheduler::TaskOutcome::Error(message) => {
                    if let Some(i) = task_errors.iter().position(|error| error == &message) {
                        task_errors.remove(i);
                    }
                    root_result = RootResult::Failed(message);
                }
                scheduler::TaskOutcome::TimedOut(message) => {
                    if let Some(i) = task_errors.iter().position(|error| error == &message) {
                        task_errors.remove(i);
                    }
                    root_result = RootResult::TimedOut;
                }
                scheduler::TaskOutcome::Cancelled => root_result = RootResult::Cancelled,
            }
        }

        // The C++ one-shot scheduler drains queued callbacks after the root
        // returns. StepReport's pending counts include ready, sleeping, and
        // yielded tasks, so don't discard their errors or timer work early.
        let pending = report.ready + report.sleeping + report.waiting;
        if !matches!(&root_result, RootResult::Running) && pending == 0 {
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        if report.ready == 0 {
            thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(1)),
            );
        }
    }

    let mut response = if Instant::now() >= deadline || matches!(&root_result, RootResult::TimedOut)
    {
        failure("execution time limit exceeded", true)
    } else {
        match root_result {
            RootResult::Completed(values) if snapshot.is_none() => match capture_values(values) {
                Ok(values) => json!({"ok":true,"values":values}),
                Err(error) => failure(error.to_string(), false),
            },
            RootResult::Completed(values) => {
                let rich = values
                    .iter()
                    .map(|value| values::result_json(&lua, value))
                    .collect::<mlua::Result<Vec<_>>>();
                match rich {
                    Ok(values) => json!({
                        "ok":true,
                        "values":values,
                        "mutations":[],
                        "movement_intents":[],
                        "output":"",
                        "output_truncated":false,
                        "joint_pose_errors":[]
                    }),
                    Err(error) => failure(format!("result capture failed: {error}"), false),
                }
            }
            RootResult::Failed(error) => failure(error, false),
            RootResult::Cancelled => failure("source task failed or was cancelled", false),
            RootResult::Running | RootResult::TimedOut => {
                failure("execution time limit exceeded", true)
            }
        }
    };
    if !task_errors.is_empty() {
        response["task_errors"] = json!(task_errors);
    }
    if snapshot.is_some() {
        let effects = std::mem::take(&mut ctx.borrow_mut().effects);
        response.as_object_mut().unwrap().extend(effects);
    }
    add_integrity(&lua, &mut response, snapshot.is_some());
    encode(response)
}

fn encode(mut response: Json) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(&response).unwrap_or_else(|_| {
        response = failure("VM worker failed", false);
        serde_json::to_vec(&response).unwrap_or_else(|_| {
            b"{\"ok\":false,\"values\":[],\"error\":\"VM worker failed\",\"timed_out\":false}"
                .to_vec()
        })
    });
    if bytes.len() > MAX_OUTPUT_BYTES {
        bytes = serde_json::to_vec(&failure("worker result exceeds output limit", false))
            .unwrap_or_default();
    }
    bytes
}
