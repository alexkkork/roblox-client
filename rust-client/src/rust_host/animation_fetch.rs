//! Yielding clip-provider requests reuse the asset loader and compact cache.
use super::*;
use crate::rust_host::scheduler;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

const MAX_PENDING: usize = 32;
const NODES_PER_FRAME: usize = 128;
const BYTES_PER_FRAME: usize = 256 * 1024;

struct Pending {
    world: WorldRef,
    reference: String,
    thread: Option<mlua::Thread>,
    deadline: Instant,
    builder: Option<animation_tree::Builder>,
}
#[derive(Default)]
struct State {
    pending: VecDeque<Pending>,
}
type StateRef = Rc<RefCell<State>>;

fn state(lua: &Lua) -> StateRef {
    if let Some(state) = lua.app_data_ref::<StateRef>() {
        return state.clone();
    }
    let state = Rc::new(RefCell::new(State::default()));
    lua.set_app_data(state.clone());
    state
}

fn getter(lua: &Lua, ctx: &WorldRef, id: &str, class: &str) -> mlua::Result<Option<Value>> {
    if !world::is_a(ctx, id, class) {
        return Ok(None);
    }
    let weak = Rc::downgrade(ctx);
    let iid = id.to_owned();
    let begin = lua.create_function(move |lua, (receiver, reference): (Value, Value)| {
        let ctx = weak.upgrade().ok_or_else(|| err("world was released"))?;
        if !world::instance_belongs_to(&receiver, &ctx)
            || world::instance_id(&receiver).as_deref() != Some(&iid)
        {
            return Err(err(
                "animation provider method requires its original receiver",
            ));
        }
        if !ctx.borrow().nodes.get(&iid).is_some_and(|node| !node.destroyed) {
            return Err(err("animation provider was destroyed"));
        }
        let Value::String(reference) = reference else {
            return Err(err("animation asset ID must be a string"));
        };
        let reference = reference.to_str()?.to_owned();
        if reference.is_empty() || reference.len() > 512 {
            return Err(err("invalid animation asset reference"));
        }
        let state = state(lua);
        if state.borrow().pending.len() >= MAX_PENDING {
            return Err(err("too many pending animation results"));
        }
        {
            let mut store = ensure_store(lua);
            if let Some(failure) = store.animation_errors.get(&reference) {
                return Err(err(failure));
            }
            request_animation(&mut store, &ctx, &reference)?;
        }
        let thread = lua.current_thread();
        scheduler::park(lua, &thread)?;
        state.borrow_mut().pending.push_back(Pending {
            world: ctx,
            reference,
            thread: Some(thread),
            deadline: Instant::now() + Duration::from_secs(60),
            builder: None,
        });
        Ok(())
    })?;
    let factory: mlua::Function = lua.load(r#"
        return function(begin)
            return function(self, reference)
                if not coroutine.isyieldable() then error('animation loading requires a yieldable coroutine', 2) end
                begin(self, reference)
                local ok, result = coroutine.yield()
                if ok ~= true then error(tostring(result or 'animation loading failed'), 2) end
                return result
            end
        end
    "#).eval()?;
    Ok(Some(Value::Function(
        factory.call::<mlua::Function>(begin)?,
    )))
}
fn get_clip(lua: &Lua, ctx: &WorldRef, id: &str) -> mlua::Result<Option<Value>> {
    getter(lua, ctx, id, "AnimationClipProvider")
}
fn get_sequence(lua: &Lua, ctx: &WorldRef, id: &str) -> mlua::Result<Option<Value>> {
    getter(lua, ctx, id, "KeyframeSequenceProvider")
}
pub(super) fn install(ctx: &WorldRef) {
    world::register_getter(ctx, "GetAnimationClipAsync", get_clip);
    world::register_getter(ctx, "GetKeyframeSequenceAsync", get_sequence);
}

fn fail(lua: &Lua, pending: &mut Pending, reason: &str) -> mlua::Result<()> {
    if let Some(thread) = pending
        .thread
        .take()
        .filter(|t| scheduler::is_waiting(lua, t))
    {
        scheduler::resume(
            lua,
            thread,
            MultiValue::from_vec(vec![
                Value::Boolean(false),
                Value::String(lua.create_string(reason)?),
            ]),
        )?;
    }
    Ok(())
}

pub(super) fn step(lua: &Lua) -> mlua::Result<()> {
    let Some(state) = lua.app_data_ref::<StateRef>().map(|s| s.clone()) else {
        return Ok(());
    };
    let mut queue = std::mem::take(&mut state.borrow_mut().pending);
    let mut bytes = queue
        .front()
        .map(|p| serde_json::to_vec(&p.world.borrow().effects).map(|v| v.len()))
        .transpose()
        .map_err(mlua::Error::external)?
        .unwrap_or(0);
    let mut waiting = 0;
    let outcome = (|| {
        for _ in 0..NODES_PER_FRAME {
            let Some(mut pending) = queue.pop_front() else {
                break;
            };
            if pending
                .thread
                .as_ref()
                .is_some_and(|t| !scheduler::is_waiting(lua, t))
            {
                pending.thread = None;
            }
            if pending.thread.is_some() && Instant::now() >= pending.deadline {
                fail(lua, &mut pending, "animation result timed out")?;
            }
            if pending.thread.is_some() && pending.builder.is_none() {
                let (clip, failure) = {
                    let store = ensure_store(lua);
                    (
                        store.animation_clips.get(&pending.reference).cloned(),
                        store.animation_errors.get(&pending.reference).cloned(),
                    )
                };
                if let Some(failure) = failure {
                    fail(lua, &mut pending, &failure)?;
                } else if let Some(clip) = clip {
                    match animation_tree::Builder::new(clip) {
                        Ok(builder) => pending.builder = Some(builder),
                        Err(error) => fail(lua, &mut pending, &error.to_string())?,
                    }
                } else {
                    queue.push_back(pending);
                    waiting += 1;
                    if waiting >= queue.len() {
                        break;
                    }
                    continue;
                }
            }
            let Some(builder) = &mut pending.builder else {
                waiting = 0;
                continue;
            };
            // Deadlines and cancellation above still run when unrelated effects
            // fill the frame. Only construction and cleanup consume this budget.
            if bytes + 72 * 1024 > BYTES_PER_FRAME {
                queue.push_back(pending);
                waiting += 1;
                if waiting >= queue.len() {
                    break;
                }
                continue;
            }
            waiting = 0;
            let done = if pending.thread.is_none() {
                builder.cleanup_one(lua, &pending.world)?
            } else {
                match builder.advance(lua, &pending.world) {
                    Ok(true) => {
                        let result =
                            world::instance(lua, &pending.world, builder.root.as_ref().unwrap())?;
                        scheduler::resume(
                            lua,
                            pending.thread.take().unwrap(),
                            MultiValue::from_vec(vec![Value::Boolean(true), result]),
                        )?;
                        true
                    }
                    Ok(false) => false,
                    Err(error) => {
                        fail(lua, &mut pending, &error.to_string())?;
                        false
                    }
                }
            };
            if let Some(row) = pending
                .world
                .borrow()
                .effects
                .get("mutations")
                .and_then(Json::as_array)
                .and_then(|rows| rows.last())
            {
                bytes += serde_json::to_vec(row)
                    .map_err(mlua::Error::external)?
                    .len();
            }
            if !done {
                queue.push_back(pending);
            }
        }
        Ok(())
    })();
    state.borrow_mut().pending.append(&mut queue);
    outcome
}

#[cfg(test)]
#[path = "animation_fetch_tests.rs"]
mod tests;
