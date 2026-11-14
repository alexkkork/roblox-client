//! Session-local Player.Idled timing. This reports genuine inactivity only;
//! it never fabricates activity or changes transport/session behavior.

use mlua::{Lua, MultiValue, Value};
use serde_json::{json, Value as Json};
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

use super::world::{self, WorldRef};

const FIRST_IDLE_MS: u64 = 120_000;
const REPEAT_IDLE_MS: u64 = 30_000;
const DISCONNECT_MS: u64 = 1_200_000;
const NATIVE_OWNER: &str = "engine:client-idle";

#[derive(Clone)]
struct IdleRef(Rc<RefCell<IdleRuntime>>);

struct IdleRuntime {
    player_id: Option<String>,
    started: Option<Instant>,
    last_activity_ms: u64,
    last_clock_ms: u64,
    next_idle_ms: u64,
    disconnected: bool,
    event_observed: bool,
    disconnect_enabled: bool,
    disconnect_requested: bool,
    notifications: u64,
    subscribed: bool,
}

impl Default for IdleRuntime {
    fn default() -> Self {
        Self {
            player_id: None,
            started: None,
            last_activity_ms: 0,
            last_clock_ms: 0,
            next_idle_ms: FIRST_IDLE_MS,
            disconnected: false,
            event_observed: false,
            disconnect_enabled: std::env::var("RCR_KEEP_OPEN_WHEN_IDLE").ok().as_deref()
                != Some("1"),
            disconnect_requested: false,
            notifications: 0,
            subscribed: false,
        }
    }
}

impl IdleRuntime {
    fn start(&mut self, player_id: String, now: Instant) {
        self.player_id = Some(player_id);
        self.started = Some(now);
        self.last_activity_ms = 0;
        self.last_clock_ms = 0;
        self.next_idle_ms = FIRST_IDLE_MS;
        self.disconnected = false;
        self.event_observed = false;
        self.disconnect_requested = false;
        self.notifications = 0;
        self.subscribed = true;
    }

    fn elapsed_at(&mut self, now: Instant) -> u64 {
        let Some(started) = self.started else {
            return 0;
        };
        let elapsed = now
            .saturating_duration_since(started)
            .as_millis()
            .min(u64::MAX as u128) as u64;
        self.last_clock_ms = self.last_clock_ms.max(elapsed);
        self.last_clock_ms
    }

    fn activity_at(&mut self, now: Instant) {
        if self.started.is_none() || self.disconnected {
            return;
        }
        let stamp = self.elapsed_at(now);
        if stamp < self.last_activity_ms {
            return;
        }
        self.last_activity_ms = stamp;
        self.next_idle_ms = FIRST_IDLE_MS;
        self.event_observed = false;
    }

    fn poll_idle(&mut self, now: Instant) -> bool {
        let stamp = self.elapsed_at(now);
        let elapsed = stamp.saturating_sub(self.last_activity_ms);
        if self.disconnected || elapsed < self.next_idle_ms {
            return false;
        }
        while self.next_idle_ms <= elapsed {
            if self.next_idle_ms == u64::MAX {
                break;
            }
            self.next_idle_ms = self
                .next_idle_ms
                .checked_add(REPEAT_IDLE_MS)
                .unwrap_or(u64::MAX);
        }
        true
    }

    fn observe_native_idle(&mut self, now: Instant) {
        let elapsed = self.elapsed_at(now).saturating_sub(self.last_activity_ms);
        self.event_observed = true;
        self.notifications = self.notifications.saturating_add(1);
        if self.disconnect_enabled && elapsed >= DISCONNECT_MS {
            self.disconnected = true;
            self.disconnect_requested = true;
        }
    }

    fn report(&mut self, now: Instant) -> Json {
        let Some(player_id) = self.player_id.clone() else {
            return Json::Null;
        };
        let now_ms = self.elapsed_at(now);
        json!({
            "version": 1,
            "player_id": player_id,
            "event_due": self.event_observed,
            "disconnect_requested": self.disconnect_requested,
            "disconnect_enabled": self.disconnect_enabled,
            "elapsed_inactive_ms": now_ms.saturating_sub(self.last_activity_ms),
            "native_notifications": self.notifications,
        })
    }
}

fn idle_state(lua: &Lua) -> mlua::Result<Rc<RefCell<IdleRuntime>>> {
    lua.app_data_ref::<IdleRef>()
        .map(|state| state.0.clone())
        .ok_or_else(|| mlua::Error::RuntimeError("idle bindings are not installed".into()))
}

/// Install the local-player signal bridge. Root calls `refresh` after actor
/// setup so this root-only native subscription cannot leak into Actor VMs.
pub fn install(lua: &Lua, ctx: &WorldRef) -> mlua::Result<()> {
    lua.set_app_data(IdleRef(Rc::new(RefCell::new(IdleRuntime::default()))));
    world::register_keyed_getter(ctx, "Idled", get_idled);
    Ok(())
}

fn get_idled(lua: &Lua, ctx: &WorldRef, id: &str, key: &str) -> mlua::Result<Option<Value>> {
    if key != "Idled" || !world::is_a(ctx, id, "Player") {
        return Ok(None);
    }
    // Ownership is installed by refresh at the root lifecycle boundary; this
    // getter only materializes the ordinary script-facing signal handle.
    Ok(Some(super::signals::signal(lua, ctx, id, "Idled")?))
}

/// Bind the engine-owned Idled connection to the current snapshot local
/// Player. Called on install and every root frame to handle join/change/remove.
pub fn refresh(lua: &Lua, ctx: &WorldRef, now: Instant) -> mlua::Result<()> {
    if super::actors::actor_id(lua).is_some() {
        return Ok(());
    }
    let local_player = ctx
        .borrow()
        .snapshot
        .get("local_player")
        .and_then(Json::as_str)
        .map(str::to_owned)
        .filter(|id| {
            world::is_a(ctx, id, "Player")
                && ctx.borrow().nodes.get(id).is_some_and(|n| !n.destroyed)
        });
    let state = idle_state(lua)?;
    let current = state.borrow().player_id.clone();
    if current == local_player {
        return Ok(());
    }
    if let Some(old) = current {
        super::signals::disconnect_owned(lua, ctx, &old, "Idled", NATIVE_OWNER)?;
    }
    if let Some(player) = local_player {
        super::signals::connect_native(lua, ctx, &player, "Idled", NATIVE_OWNER)?;
        state.borrow_mut().start(player, now);
    } else {
        let disconnect_enabled = state.borrow().disconnect_enabled;
        *state.borrow_mut() = IdleRuntime {
            disconnect_enabled,
            ..IdleRuntime::default()
        };
    }
    Ok(())
}

/// Record activity only for non-synthetic keyboard, mouse, and touch events.
/// `phase` is the normalized UserInputState name.
pub fn record_activity(lua: &Lua, input_type: &str, phase: &str, synthetic: bool) {
    if synthetic || phase == "Cancel" {
        return;
    }
    if !matches!(
        input_type,
        "Keyboard"
            | "MouseButton1"
            | "MouseButton2"
            | "MouseButton3"
            | "MouseWheel"
            | "MouseMovement"
            | "Touch"
    ) {
        return;
    }
    let Some(state) = lua.app_data_ref::<IdleRef>().map(|state| state.0.clone()) else {
        return;
    };
    state.borrow_mut().activity_at(Instant::now());
}

/// Advance the timer and enqueue an ordinary local `Player.Idled` signal.
/// There is no network close or authentication behavior in this module.
pub fn step(lua: &Lua, ctx: &WorldRef, now: Instant) -> mlua::Result<()> {
    refresh(lua, ctx, now)?;
    let state = idle_state(lua)?;
    let (player, elapsed_seconds, fired, native_enabled) = {
        let mut state = state.borrow_mut();
        let Some(player) = state.player_id.clone() else {
            return Ok(());
        };
        let fired = state.poll_idle(now);
        let elapsed_seconds =
            state.last_clock_ms.saturating_sub(state.last_activity_ms) as f64 / 1000.0;
        let native_enabled = state.subscribed
            && super::signals::owned_status(lua, ctx, &player, "Idled", NATIVE_OWNER)?
                .is_some_and(|(connected, enabled)| connected && enabled);
        (player, elapsed_seconds, fired, native_enabled)
    };
    if fired {
        super::signals::fire(
            lua,
            ctx,
            &player,
            "Idled",
            MultiValue::from_vec(vec![Value::Number(elapsed_seconds)]),
        )?;
        if native_enabled {
            idle_state(lua)?.borrow_mut().observe_native_idle(now);
        }
    }
    Ok(())
}

/// Report compatible with the legacy `idle_control` response field.
pub fn report(lua: &Lua, now: Instant) -> Json {
    let Some(state) = lua.app_data_ref::<IdleRef>().map(|state| state.0.clone()) else {
        return Json::Null;
    };
    let report = state.borrow_mut().report(now);
    report
}

#[cfg(test)]
#[path = "idle_tests.rs"]
mod tests;
