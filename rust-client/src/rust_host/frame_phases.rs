//! A shared frame order for the root VM and Actor VMs.
use super::{movement, playback, scheduler, signals, world};
use mlua::{Lua, MultiValue, Value};
use std::time::{Duration, Instant};

pub(crate) fn step(
    lua: &Lua,
    ctx: &world::WorldRef,
    scheduler: &scheduler::SchedulerRef,
    started: Instant,
    elapsed: f64,
    budget: Duration,
) -> mlua::Result<scheduler::StepReport> {
    let service = ctx.borrow().services.get("RunService").cloned();
    let fire = |events: &[&str]| -> mlua::Result<()> {
        if let Some(id) = &service {
            for event in events {
                let args = if *event == "Stepped" {
                    MultiValue::from_vec(vec![
                        Value::Number(started.elapsed().as_secs_f64()),
                        Value::Number(elapsed),
                    ])
                } else {
                    MultiValue::from_vec(vec![Value::Number(elapsed)])
                };
                signals::fire(lua, ctx, id, event, args)?;
            }
        }
        Ok(())
    };
    // Charge VM execution across phases. Engine bookkeeping must not consume
    // a newly submitted script's entire 1 ms execution allowance.
    let animation = scheduler::step_phase(lua, scheduler, budget, || fire(&["PreAnimation"]))?;
    playback::step(lua, ctx, elapsed)?;
    let remaining = budget.saturating_sub(animation.execution_time);
    let simulation = scheduler::step_phase(lua, scheduler, remaining, || {
        fire(&["PreSimulation", "Stepped"])
    })?;
    movement::flush_joint_poses(lua, ctx)?;
    world::refresh_derived_properties(lua)?;
    fire(&["PostSimulation", "Heartbeat", "PreRender", "RenderStepped"])?;
    let mut result = scheduler::step(
        lua,
        scheduler,
        Instant::now(),
        remaining.saturating_sub(simulation.execution_time),
    );
    for phase in [animation, simulation] {
        result.execution_time += phase.execution_time;
        result.resumed += phase.resumed;
        result.completed += phase.completed;
        result.preempted += phase.preempted;
        result.errors.extend(phase.errors);
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc, time::Duration};
    #[test]
    fn phase_callbacks_do_not_wake_ordinary_waiters_early() {
        let lua = Lua::new();
        let scheduler = Rc::new(RefCell::new(scheduler::Scheduler::new()));
        scheduler::install(&lua, &scheduler).unwrap();
        lua.load("order={};task.spawn(function() table.insert(order,'ordinary') end);task.delay(0,function() table.insert(order,'sleeper') end)").exec().unwrap();
        scheduler::step_phase(&lua, &scheduler, Duration::from_millis(30), || {
            scheduler::enqueue(
                &lua,
                lua.load("table.insert(order,'phase')").into_function()?,
                MultiValue::new(),
            )?;
            Ok(())
        })
        .unwrap();
        lua.load("assert(table.concat(order,',')=='phase')")
            .exec()
            .unwrap();
        scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(30));
        lua.load("assert(table.concat(order,',')=='phase,ordinary,sleeper')")
            .exec()
            .unwrap();
    }
    #[test]
    fn phase_enqueue_failure_restores_existing_and_partial_callback_queues() {
        let lua = Lua::new();
        let scheduler = Rc::new(RefCell::new(scheduler::Scheduler::new()));
        scheduler::install(&lua, &scheduler).unwrap();
        lua.load(
            "count=0;task.spawn(function() count+=1 end);task.delay(0,function() count+=10 end)",
        )
        .exec()
        .unwrap();
        let result = scheduler::step_phase(&lua, &scheduler, Duration::ZERO, || {
            scheduler::enqueue(
                &lua,
                lua.load("count+=100").into_function()?,
                MultiValue::new(),
            )?;
            Err(mlua::Error::RuntimeError("enqueue failed".into()))
        });
        assert!(result.is_err());
        assert_eq!(scheduler::snapshot(&scheduler).active_tasks, 3);
        scheduler::step(&lua, &scheduler, Instant::now(), Duration::from_millis(30));
        lua.load("assert(count==111)").exec().unwrap();
    }
}
