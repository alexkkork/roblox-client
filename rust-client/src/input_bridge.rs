//! Validated local input arbitration for one shared browser surface.
//!
//! This module plans owner-thread actions only. A caller applies each action to
//! the local Luau runtime and commits the plan after successful acknowledgments.
//! It never treats local input as server movement.

use serde_json::{json, Map, Value};
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

const KEY_CODES: &str = include_str!("../../ui/keyboard-codes.json");
const DEFAULT_TTL: Duration = Duration::from_secs(2);
const MOVE_TTL: Duration = Duration::from_millis(650);

#[derive(Clone, Debug, PartialEq)]
pub enum OwnerAction {
    InputCapabilities(Value),
    InputEvents(Vec<Value>),
    Display(Value),
    ExecuteLocalLuau { source: String, timeout_ms: u32 },
}

#[derive(Clone, Debug, PartialEq)]
pub struct InputPlan {
    pub actions: Vec<OwnerAction>,
    pub response: Value,
    next: InputBridge,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputError(pub String);

impl std::fmt::Display for InputError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InputError {}

#[derive(Clone, Debug, PartialEq)]
pub struct InputBridge {
    ttl: Duration,
    client_id: Option<String>,
    lease: Option<String>,
    focused: bool,
    held: BTreeSet<String>,
    mouse_held: BTreeSet<String>,
    mouse_location: [f64; 2],
    deadline: Option<Instant>,
    last_sequence: u64,
    last_request: Option<Value>,
    last_response: Option<Value>,
    original_keyboard: bool,
    original_mouse: bool,
    move_client: Option<String>,
    move_sequence: u64,
    move_deadline: Option<Instant>,
    moving: bool,
}

impl Default for InputBridge {
    fn default() -> Self {
        Self::new(DEFAULT_TTL, false, false)
    }
}

impl InputBridge {
    pub fn new(ttl: Duration, keyboard_enabled: bool, mouse_enabled: bool) -> Self {
        Self {
            ttl: ttl.clamp(Duration::from_secs(1), Duration::from_secs(10)),
            client_id: None,
            lease: None,
            focused: false,
            held: BTreeSet::new(),
            mouse_held: BTreeSet::new(),
            mouse_location: [0.0, 0.0],
            deadline: None,
            last_sequence: 0,
            last_request: None,
            last_response: None,
            original_keyboard: keyboard_enabled,
            original_mouse: mouse_enabled,
            move_client: None,
            move_sequence: 0,
            move_deadline: None,
            moving: false,
        }
    }

    /// Plan a browser keyboard/mouse request. Commit only after every listed
    /// native update succeeds, so a rejected update cannot advance its lease.
    pub fn plan_keyboard(&self, request: &Value, now: Instant) -> Result<InputPlan, InputError> {
        validate_keyboard(request)?;
        let mut next = self.clone();
        let mut actions = Vec::new();
        next.expire_keyboard(now, &mut actions)?;

        let action = request["action"].as_str().unwrap();
        let client = request["client_id"].as_str().unwrap();
        let sequence = request["sequence"].as_u64().unwrap();
        if next.client_id.as_deref() == Some(client) {
            if sequence == next.last_sequence {
                if next.last_request.as_ref() != Some(request) {
                    return Err(InputError(
                        "Keyboard sequence was reused for different input".into(),
                    ));
                }
                next.deadline = Some(now + next.ttl);
                let response = next
                    .last_response
                    .clone()
                    .unwrap_or_else(|| json!({"ok":true}));
                return Ok(InputPlan {
                    actions,
                    response,
                    next,
                });
            }
            if sequence < next.last_sequence {
                return Err(InputError("Keyboard request arrived out of order".into()));
            }
        }

        let response = match action {
            "attach" => {
                if next
                    .client_id
                    .as_deref()
                    .is_some_and(|owner| owner != client)
                {
                    if next.focused {
                        return Err(InputError(
                            "Keyboard controls are active in another browser page".into(),
                        ));
                    }
                    next.detach_keyboard(&mut actions);
                }
                if next.client_id.is_none() {
                    actions.push(OwnerAction::InputCapabilities(
                        json!({"KeyboardEnabled":true,"MouseEnabled":true}),
                    ));
                    next.client_id = Some(client.to_owned());
                    next.lease = Some(random_lease()?);
                }
                next.focused = true;
                json!({"ok":true,"values":[],"lease":next.lease,"keyboard_attached":true,"mouse_attached":true,"focused":true,"state":"accepted_local_only"})
            }
            "events" | "release" | "detach" | "heartbeat" => {
                if next.client_id.as_deref() != Some(client)
                    || request["lease"].as_str() != next.lease.as_deref()
                {
                    return Err(InputError(
                        "Keyboard lease expired; focus the controls again".into(),
                    ));
                }
                match action {
                    "events" => {
                        if !next.focused {
                            return Err(InputError(
                                "Focus the keyboard controls before sending input".into(),
                            ));
                        }
                        let (events, held, buttons, location) =
                            next.plan_events(&request["events"])?;
                        actions.push(OwnerAction::InputEvents(events));
                        next.held = held;
                        next.mouse_held = buttons;
                        next.mouse_location = location;
                        json!({"ok":true,"values":[],"state":"accepted_local_only"})
                    }
                    "release" => {
                        next.cancel_inputs(&mut actions);
                        next.focused = false;
                        json!({"ok":true,"values":[],"focused":false,"state":"accepted_local_only"})
                    }
                    "detach" => {
                        next.detach_keyboard(&mut actions);
                        json!({"ok":true,"values":[],"keyboard_attached":false,"state":"accepted_local_only"})
                    }
                    _ => {
                        json!({"ok":true,"values":[],"focused":next.focused,"state":"accepted_local_only"})
                    }
                }
            }
            _ => unreachable!(),
        };
        next.deadline = Some(now + next.ttl);
        next.last_sequence = sequence;
        next.last_request = Some(request.clone());
        next.last_response = Some(response.clone());
        Ok(InputPlan {
            actions,
            response,
            next,
        })
    }

    pub fn plan_keyboard_expiry(&self, now: Instant) -> InputPlan {
        let mut next = self.clone();
        let mut actions = Vec::new();
        next.expire_keyboard(now, &mut actions).ok();
        InputPlan {
            actions,
            response: json!({"ok":true,"state":"accepted_local_only"}),
            next,
        }
    }

    /// Validate display geometry in UI pixels. It remains a local runtime
    /// setting and is not written to the replicated world.
    pub fn plan_display(profile: &Value) -> Result<OwnerAction, InputError> {
        let obj = profile
            .as_object()
            .ok_or_else(|| InputError("Display profile must be an object".into()))?;
        if !obj.contains_key("size")
            || obj.keys().any(|k| {
                !["size", "device_insets", "core_insets", "topbar_insets"].contains(&k.as_str())
            })
        {
            return Err(InputError(
                "Display requires size and optional safe-area margins".into(),
            ));
        }
        let size = pair(
            obj.get("size"),
            "Display dimensions must be finite numbers in 0..32768",
        )?;
        let device = obj
            .get("device_insets")
            .map(|x| quad(Some(x), "Invalid device insets"))
            .transpose()?
            .unwrap_or([0.0; 4]);
        let core = obj
            .get("core_insets")
            .map(|x| quad(Some(x), "Invalid core insets"))
            .transpose()?
            .unwrap_or(device);
        let topbar = obj
            .get("topbar_insets")
            .map(|x| quad(Some(x), "Invalid topbar insets"))
            .transpose()?;
        for insets in [Some(device), Some(core), topbar].into_iter().flatten() {
            if insets[0] + insets[2] > size[0] || insets[1] + insets[3] > size[1] {
                return Err(InputError("Display insets exceed surface size".into()));
            }
            if insets != device && insets.iter().zip(device).any(|(a, b)| *a < b) {
                return Err(InputError(
                    "UI safe area must be inside device safe area".into(),
                ));
            }
        }
        let mut normalized = obj.clone();
        normalized.entry("device_insets").or_insert(json!(device));
        normalized.entry("core_insets").or_insert(json!(core));
        Ok(OwnerAction::Display(Value::Object(normalized)))
    }

    /// Plan one local Humanoid:Move call. The caller must pass `Some(true)`
    /// only after resolving the exact local player's current Humanoid.
    pub fn plan_movement(
        &self,
        request: &Value,
        now: Instant,
        own_humanoid: Option<bool>,
    ) -> Result<InputPlan, InputError> {
        validate_movement(request)?;
        if own_humanoid != Some(true) {
            return Err(InputError("Own character Humanoid is unavailable".into()));
        }
        let mut next = self.clone();
        let mut actions = Vec::new();
        next.expire_movement(now, Some(true), &mut actions);
        let client = request["client"].as_str().unwrap();
        let sequence = request["sequence"].as_u64().unwrap();
        if next
            .move_client
            .as_deref()
            .is_some_and(|owner| owner != client)
            && next.moving
        {
            return Err(InputError(
                "Another display controls movement; release its controls first".into(),
            ));
        }
        if next.move_client.as_deref() == Some(client) && sequence <= next.move_sequence {
            return Err(InputError("Stale movement sequence".into()));
        }
        let v = request["direction"].as_array().unwrap();
        let x = v[0].as_f64().unwrap();
        let z = v[2].as_f64().unwrap();
        let length = x.hypot(z);
        let scale = length.max(1.0);
        let direction = [x / scale, 0.0, z / scale];
        actions.push(OwnerAction::ExecuteLocalLuau {
            source: move_source(direction),
            timeout_ms: 30,
        });
        next.move_client = Some(client.to_owned());
        next.move_sequence = sequence;
        next.move_deadline = Some(now + MOVE_TTL);
        next.moving = length > 0.0;
        Ok(InputPlan {
            actions,
            response: json!({"ok":true,"values":[],"state":"accepted_local_only","server_movement_confirmed":false}),
            next,
        })
    }

    pub fn plan_expiry(&self, now: Instant, own_humanoid: Option<bool>) -> InputPlan {
        let mut next = self.clone();
        let mut actions = Vec::new();
        next.expire_keyboard(now, &mut actions).ok();
        next.expire_movement(now, own_humanoid, &mut actions);
        InputPlan {
            actions,
            response: json!({"ok":true,"state":"accepted_local_only","server_movement_confirmed":false}),
            next,
        }
    }

    pub fn plan_close(&self, own_humanoid: Option<bool>) -> InputPlan {
        let mut next = self.clone();
        let mut actions = Vec::new();
        next.detach_keyboard(&mut actions);
        if next.moving {
            if own_humanoid == Some(true) {
                actions.push(OwnerAction::ExecuteLocalLuau {
                    source: stop_source(),
                    timeout_ms: 30,
                });
            }
            next.moving = false;
            next.move_deadline = None;
        }
        InputPlan {
            actions,
            response: json!({"ok":true,"state":"accepted_local_only"}),
            next,
        }
    }

    /// Commit a plan after every native action has returned success.
    pub fn commit(&mut self, plan: InputPlan) {
        *self = plan.next;
    }

    fn plan_events(
        &self,
        input: &Value,
    ) -> Result<(Vec<Value>, BTreeSet<String>, BTreeSet<String>, [f64; 2]), InputError> {
        let mut held = self.held.clone();
        let mut buttons = self.mouse_held.clone();
        let mut location = self.mouse_location;
        let mut out = Vec::new();
        for event in input
            .as_array()
            .ok_or_else(|| InputError("Invalid input events".into()))?
        {
            if event.get("input_type").is_some() {
                let kind = event["input_type"].as_str().unwrap();
                let pos = vec_numbers(
                    &event["mouse_location"],
                    2,
                    "Mouse coordinates must be bounded finite pixel values",
                )?;
                location = [pos[0], pos[1]];
                if !matches!(kind, "MouseButton1" | "MouseButton2" | "MouseButton3") {
                    let mut e = event.as_object().unwrap().clone();
                    e.insert("processed".into(), Value::Bool(false));
                    out.push(Value::Object(e));
                    continue;
                }
                let active = &mut buttons;
                input_transition(active, kind, event["phase"].as_str().unwrap())?;
                let mut e = event.as_object().unwrap().clone();
                e.insert("processed".into(), Value::Bool(false));
                out.push(Value::Object(e));
            } else {
                let key = event["key_code"].as_str().unwrap();
                input_transition(&mut held, key, event["phase"].as_str().unwrap())?;
                if held.len() > 64 {
                    return Err(InputError("Too many held keyboard keys".into()));
                }
                let mut e = event.as_object().unwrap().clone();
                e.insert("input_type".into(), Value::String("Keyboard".into()));
                e.insert("processed".into(), Value::Bool(false));
                out.push(Value::Object(e));
            }
        }
        Ok((out, held, buttons, location))
    }

    fn cancel_inputs(&mut self, actions: &mut Vec<OwnerAction>) {
        let mut events = Vec::new();
        for key in &self.held {
            events.push(
                json!({"phase":"cancel","input_type":"Keyboard","key_code":key,"processed":false}),
            );
        }
        for button in &self.mouse_held {
            events.push(json!({"phase":"cancel","input_type":button,"position":[self.mouse_location[0],self.mouse_location[1],0],"delta":[0,0,0],"mouse_location":self.mouse_location,"processed":false}));
        }
        if !events.is_empty() {
            actions.push(OwnerAction::InputEvents(events));
        }
        self.held.clear();
        self.mouse_held.clear();
    }

    fn detach_keyboard(&mut self, actions: &mut Vec<OwnerAction>) {
        if self.client_id.is_none() {
            return;
        }
        self.cancel_inputs(actions);
        actions.push(OwnerAction::InputCapabilities(
            json!({"KeyboardEnabled":self.original_keyboard,"MouseEnabled":self.original_mouse}),
        ));
        self.client_id = None;
        self.lease = None;
        self.focused = false;
        self.deadline = None;
        self.last_sequence = 0;
        self.last_request = None;
        self.last_response = None;
    }

    fn expire_keyboard(
        &mut self,
        now: Instant,
        actions: &mut Vec<OwnerAction>,
    ) -> Result<(), InputError> {
        if self.deadline.is_some_and(|deadline| now >= deadline) {
            self.detach_keyboard(actions);
        }
        Ok(())
    }

    fn expire_movement(
        &mut self,
        now: Instant,
        own_humanoid: Option<bool>,
        actions: &mut Vec<OwnerAction>,
    ) {
        if self.moving && self.move_deadline.is_some_and(|deadline| now >= deadline) {
            if own_humanoid == Some(true) {
                actions.push(OwnerAction::ExecuteLocalLuau {
                    source: stop_source(),
                    timeout_ms: 30,
                });
            }
            self.moving = false;
            self.move_deadline = None;
        }
    }
}

fn input_transition(
    active: &mut BTreeSet<String>,
    key: &str,
    phase: &str,
) -> Result<(), InputError> {
    match phase {
        "begin" if active.insert(key.to_owned()) => Ok(()),
        "begin" => Err(InputError("Input is already down".into())),
        "end" | "cancel" if active.remove(key) => Ok(()),
        "end" | "cancel" => Err(InputError("Input is not down".into())),
        _ => Err(InputError("Invalid input phase".into())),
    }
}

fn validate_keyboard(request: &Value) -> Result<(), InputError> {
    let obj = request
        .as_object()
        .ok_or_else(|| InputError("Keyboard request must be an object".into()))?;
    let action = obj
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| InputError("Invalid keyboard action".into()))?;
    if !matches!(
        action,
        "attach" | "events" | "release" | "heartbeat" | "detach"
    ) {
        return Err(InputError("Invalid keyboard action".into()));
    }
    let expected: BTreeSet<&str> = if action == "attach" {
        ["action", "client_id", "sequence"].into()
    } else if action == "events" {
        ["action", "client_id", "sequence", "lease", "events"].into()
    } else {
        ["action", "client_id", "sequence", "lease"].into()
    };
    if obj.keys().any(|k| !expected.contains(k.as_str())) || obj.len() != expected.len() {
        return Err(InputError("Invalid keyboard request fields".into()));
    }
    let client = obj
        .get("client_id")
        .and_then(Value::as_str)
        .ok_or_else(|| InputError("Invalid keyboard client".into()))?;
    if !valid_client(client) {
        return Err(InputError("Invalid keyboard client".into()));
    }
    if obj["sequence"]
        .as_u64()
        .is_none_or(|n| n < 1 || n >= (1u64 << 53))
    {
        return Err(InputError("Invalid keyboard sequence".into()));
    }
    if action != "attach"
        && !obj
            .get("lease")
            .and_then(Value::as_str)
            .is_some_and(valid_client)
    {
        return Err(InputError("Invalid keyboard lease".into()));
    }
    if action == "events" {
        let events = obj["events"]
            .as_array()
            .filter(|v| !v.is_empty() && v.len() <= 64)
            .ok_or_else(|| InputError("Keyboard batch must contain 1 to 64 events".into()))?;
        for e in events {
            validate_event(e)?;
        }
    }
    Ok(())
}

fn validate_event(event: &Value) -> Result<(), InputError> {
    let obj = event
        .as_object()
        .ok_or_else(|| InputError("Invalid input event".into()))?;
    if let Some(kind) = obj.get("input_type").and_then(Value::as_str) {
        let allowed = [
            "MouseButton1",
            "MouseButton2",
            "MouseButton3",
            "MouseMovement",
            "MouseWheel",
        ];
        if !allowed.contains(&kind)
            || obj.len() != 5
            || obj.keys().any(|k| {
                !["phase", "input_type", "position", "delta", "mouse_location"]
                    .contains(&k.as_str())
            })
        {
            return Err(InputError("Invalid mouse event fields".into()));
        }
        let phase = obj.get("phase").and_then(Value::as_str).unwrap_or("");
        let valid_phase = if matches!(kind, "MouseButton1" | "MouseButton2" | "MouseButton3") {
            ["begin", "end", "cancel"].contains(&phase)
        } else {
            phase == "change"
        };
        if !valid_phase {
            return Err(InputError("Invalid mouse event phase".into()));
        }
        vec_numbers(
            &event["position"],
            3,
            "Mouse coordinates must be bounded finite pixel values",
        )?;
        vec_numbers(
            &event["delta"],
            3,
            "Mouse coordinates must be bounded finite pixel values",
        )?;
        vec_numbers(
            &event["mouse_location"],
            2,
            "Mouse coordinates must be bounded finite pixel values",
        )?;
    } else {
        if obj.len() != 2
            || obj
                .keys()
                .any(|k| !["phase", "key_code"].contains(&k.as_str()))
        {
            return Err(InputError("Invalid keyboard event fields".into()));
        }
        let phase = obj.get("phase").and_then(Value::as_str).unwrap_or("");
        if !["begin", "end", "cancel"].contains(&phase) {
            return Err(InputError("Invalid keyboard phase".into()));
        }
        let key = obj
            .get("key_code")
            .and_then(Value::as_str)
            .ok_or_else(|| InputError("Invalid keyboard key".into()))?;
        let codes: Map<String, Value> = serde_json::from_str(KEY_CODES)
            .map_err(|_| InputError("Keyboard code table unavailable".into()))?;
        if !codes.values().any(|v| v.as_str() == Some(key)) {
            return Err(InputError("Invalid keyboard key".into()));
        }
    }
    Ok(())
}

fn validate_movement(request: &Value) -> Result<(), InputError> {
    let obj = request
        .as_object()
        .ok_or_else(|| InputError("Invalid movement request".into()))?;
    if obj.len() != 3
        || obj
            .keys()
            .any(|k| !["client", "sequence", "direction"].contains(&k.as_str()))
    {
        return Err(InputError("Invalid movement request".into()));
    }
    if !obj["client"].as_str().is_some_and(|v| {
        v.len() == 32
            && v.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }) {
        return Err(InputError("Invalid movement client".into()));
    }
    if obj["sequence"]
        .as_u64()
        .is_none_or(|v| v < 1 || v >= (1u64 << 53))
    {
        return Err(InputError("Invalid movement sequence".into()));
    }
    let direction = vec_numbers(&obj["direction"], 3, "Invalid movement direction")?;
    if direction[1] != 0.0 || direction.iter().any(|v| v.abs() > 1.0) {
        return Err(InputError("Invalid movement direction".into()));
    }
    Ok(())
}

fn valid_client(value: &str) -> bool {
    (16..=64).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
fn vec_numbers(value: &Value, len: usize, message: &str) -> Result<Vec<f64>, InputError> {
    let v = value
        .as_array()
        .filter(|v| v.len() == len)
        .ok_or_else(|| InputError(message.into()))?;
    let out: Result<Vec<_>, _> = v
        .iter()
        .map(|x| {
            x.as_f64()
                .filter(|n| n.is_finite() && n.abs() <= 10_000_000.0)
                .ok_or_else(|| InputError(message.into()))
        })
        .collect();
    out
}
fn pair(value: Option<&Value>, message: &str) -> Result<[f64; 2], InputError> {
    let n = vec_numbers(value.ok_or_else(|| InputError(message.into()))?, 2, message)?;
    if n.iter().any(|x| *x < 0.0 || *x > 32768.0) {
        return Err(InputError(message.into()));
    }
    Ok([n[0], n[1]])
}
fn quad(value: Option<&Value>, message: &str) -> Result<[f64; 4], InputError> {
    let n = vec_numbers(value.ok_or_else(|| InputError(message.into()))?, 4, message)?;
    if n.iter().any(|x| *x < 0.0 || *x > 32768.0) {
        return Err(InputError(message.into()));
    }
    Ok([n[0], n[1], n[2], n[3]])
}
fn random_lease() -> Result<String, InputError> {
    let mut bytes = [0u8; 24];
    rove_client_core::entropy::fill(&mut bytes)
        .map_err(|_| InputError("Could not create a secure keyboard lease".into()))?;
    Ok(base64::Engine::encode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        bytes,
    ))
}
fn move_source(direction: [f64; 3]) -> String {
    format!("local p=game:GetService(\"Players\").LocalPlayer local c=p and p.Character local h=c and c:FindFirstChildOfClass(\"Humanoid\") if not h or h.Parent~=c then error(\"Own character Humanoid is unavailable\") end h:Move(Vector3.new({:.8},0,{:.8}),false) return true",direction[0],direction[2])
}

fn stop_source() -> String {
    // The avatar may disappear between the last proof and the lease timeout.
    // A stop is best-effort local cleanup, so a missing Humanoid is a no-op.
    "local p=game:GetService(\"Players\").LocalPlayer local c=p and p.Character local h=c and c:FindFirstChildOfClass(\"Humanoid\") if h and h.Parent==c then h:Move(Vector3.new(0,0,0),false) end return true".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn attach() -> Value {
        json!({"action":"attach","client_id":"browser_client_12345","sequence":1})
    }
    fn movement(seq: u64, direction: Value) -> Value {
        json!({"client":"0123456789abcdef0123456789abcdef","sequence":seq,"direction":direction})
    }
    #[test]
    fn attach_commits_only_explicitly_and_duplicate_is_idempotent() {
        let now = Instant::now();
        let bridge = InputBridge::default();
        let plan = bridge.plan_keyboard(&attach(), now).unwrap();
        assert_eq!(bridge.client_id, None);
        assert_eq!(
            plan.actions,
            vec![OwnerAction::InputCapabilities(
                json!({"KeyboardEnabled":true,"MouseEnabled":true})
            )]
        );
        let mut bridge = bridge;
        bridge.commit(plan.clone());
        assert_eq!(bridge.client_id.as_deref(), Some("browser_client_12345"));
        assert_eq!(
            bridge
                .plan_keyboard(&attach(), now + Duration::from_millis(1))
                .unwrap()
                .response,
            plan.response
        );
    }
    #[test]
    fn keyboard_expiry_cancels_held_key_and_restores_capabilities() {
        let now = Instant::now();
        let mut bridge = InputBridge::default();
        let p = bridge.plan_keyboard(&attach(), now).unwrap();
        bridge.commit(p);
        let lease = bridge.lease.clone().unwrap();
        let event = json!({"action":"events","client_id":"browser_client_12345","sequence":2,"lease":lease,"events":[{"phase":"begin","key_code":"W"}]});
        let p = bridge
            .plan_keyboard(&event, now + Duration::from_millis(1))
            .unwrap();
        bridge.commit(p);
        let p = bridge.plan_keyboard_expiry(now + Duration::from_secs(3));
        assert_eq!(p.actions.len(), 2);
        assert!(
            matches!(&p.actions[0],OwnerAction::InputEvents(events) if events[0]["phase"]=="cancel")
        );
        assert_eq!(
            p.actions[1],
            OwnerAction::InputCapabilities(json!({"KeyboardEnabled":false,"MouseEnabled":false}))
        );
    }
    #[test]
    fn display_insets_are_validated_and_normalized() {
        let a = InputBridge::plan_display(&json!({"size":[800,600],"device_insets":[1,2,3,4]}))
            .unwrap();
        assert_eq!(
            a,
            OwnerAction::Display(
                json!({"size":[800,600],"device_insets":[1,2,3,4],"core_insets":[1.0,2.0,3.0,4.0]})
            )
        );
        assert!(
            InputBridge::plan_display(&json!({"size":[10,10],"device_insets":[6,0,6,0]})).is_err()
        );
    }
    #[test]
    fn movement_requires_proof_and_expires_to_local_zero() {
        let now = Instant::now();
        let req = movement(1, json!([0.8, 0.0, 0.8]));
        assert!(InputBridge::default()
            .plan_movement(&req, now, None)
            .is_err());
        let mut bridge = InputBridge::default();
        let p = bridge.plan_movement(&req, now, Some(true)).unwrap();
        assert_eq!(p.response["server_movement_confirmed"], false);
        bridge.commit(p);
        let p = bridge.plan_expiry(now + Duration::from_millis(700), Some(true));
        assert_eq!(p.actions.len(), 1);
        assert!(
            matches!(&p.actions[0],OwnerAction::ExecuteLocalLuau{source,..} if source.contains("if h and h.Parent==c") && source.contains("Vector3.new(0,0,0)"))
        );
    }
    #[test]
    fn movement_arbitration_rejects_second_active_display_and_normalizes() {
        let now = Instant::now();
        let mut bridge = InputBridge::default();
        let p = bridge
            .plan_movement(&movement(1, json!([1.0, 0.0, 1.0])), now, Some(true))
            .unwrap();
        assert!(
            matches!(&p.actions[0],OwnerAction::ExecuteLocalLuau{source,..} if source.contains("Vector3.new(0.70710678,0,0.70710678)"))
        );
        bridge.commit(p);
        assert!(bridge.plan_movement(&json!({"client":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","sequence":1,"direction":[0,0,1]}),now,Some(true)).is_err());
    }
}
