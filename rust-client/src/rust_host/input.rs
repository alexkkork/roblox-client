//! Local input state for one Luau VM. Inputs are simulated inside this
//! runtime; this module never injects events into the host desktop.

use mlua::{Function, Lua, MultiValue, Value};
use serde_json::{json, Value as Json};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc, time::Instant};

use super::{
    gui,
    world::{self, Node, WorldRef},
};

#[cfg(test)]
#[path = "input_tests.rs"]
mod tests;

const MAX_BATCH: usize = 128;
const MAX_HELD: usize = 128;

#[derive(Clone)]
struct Contact {
    id: String,
    input_type: String,
    key: String,
}

#[derive(Default)]
struct InputState {
    capabilities: BTreeMap<String, bool>,
    contacts: BTreeMap<String, Contact>,
    continuous: BTreeMap<String, String>,
    last_type: String,
    preferred: String,
    mouse: [f32; 2],
    frame_delta: [f32; 2],
    next_id: u64,
    properties: BTreeMap<String, Json>,
    gui_pointer_target: Option<String>,
    context_actions: BTreeMap<String, ContextBinding>,
    next_context_sequence: u64,
    gui_pointer_position: [f64; 2],
    player_modes: BTreeMap<String, PlayerModeState>,
    recording_started: Option<Instant>,
    recording_duration: f64,
    recording_events: Vec<Json>,
    recording_bytes: usize,
    recording_initial_mouse: [f32; 2],
    additional_lua_state: String,
    recording_last_error: String,
    playback_events: Vec<Json>,
    playback_duration: f64,
    playback_started_at: Option<Instant>,
    playback_cursor: usize,
    playback_additional_state: String,
    playback_last_error: String,
    playback_owned: BTreeMap<String, Contact>,
    playback_owned_gui_pointer: Option<String>,
}

#[derive(Clone, Default)]
struct PlayerModeState {
    registered: [bool; 4],
    values: [Vec<i32>; 4],
}

#[derive(Clone)]
struct ContextBinding {
    callback: Function,
    priority: i32,
    inputs: Vec<(String, String)>,
    sequence: u64,
}
#[derive(Clone)]
struct ContextDispatcher(Function);

#[derive(Clone)]
struct InputStateRef(Rc<RefCell<InputState>>);

fn state(lua: &Lua) -> mlua::Result<Rc<RefCell<InputState>>> {
    lua.app_data_ref::<InputStateRef>()
        .map(|s| s.0.clone())
        .ok_or_else(|| mlua::Error::RuntimeError("input bindings are not installed".into()))
}

fn enum_json_lua(lua: &Lua, kind: &str, name: &str) -> mlua::Result<Json> {
    let item = super::enums::enum_item(lua, kind, name)?;
    Ok(
        json!({"$enum":{"type":kind,"name":super::enums::enum_item_name(&item)?,"value":super::enums::enum_item_value(&item)?}}),
    )
}

fn ensure_service(ctx: &WorldRef, class: &str, key: &str) -> bool {
    world::is_a(ctx, key, class)
}

fn getter(lua: &Lua, ctx: &WorldRef, id: &str, key: &str) -> mlua::Result<Option<Value>> {
    if !ensure_service(ctx, "UserInputService", id) {
        return Ok(None);
    }
    let current = state(lua)?;
    let s = current.borrow();
    if let Some(value) = s.capabilities.get(key) {
        return Ok(Some(Value::Boolean(*value)));
    }
    let value = match key {
        "PreferredInput" => Some(enum_json_lua(
            lua,
            "PreferredInput",
            if s.preferred.is_empty() {
                "KeyboardAndMouse"
            } else {
                &s.preferred
            },
        )?),
        "LastInputType" => Some(enum_json_lua(
            lua,
            "UserInputType",
            if s.last_type.is_empty() {
                "None"
            } else {
                &s.last_type
            },
        )?),
        "MouseBehavior" => Some(s.properties.get(key).cloned().unwrap_or(enum_json_lua(
            lua,
            "MouseBehavior",
            "Default",
        )?)),
        "MouseIconEnabled" => Some(s.properties.get(key).cloned().unwrap_or(json!(true))),
        "MouseDeltaSensitivity" => Some(s.properties.get(key).cloned().unwrap_or(json!(1.0))),
        _ => None,
    };
    value.map(|v| super::values::from_json(lua, &v)).transpose()
}

fn setter(lua: &Lua, ctx: &WorldRef, id: &str, value: Value, key: &str) -> mlua::Result<bool> {
    if !ensure_service(ctx, "UserInputService", id) {
        return Ok(false);
    }
    let v = super::values::to_json(lua, &value)?;
    let current = state(lua)?;
    let mut s = current.borrow_mut();
    if key == "MouseBehavior" {
        if v.get("$enum")
            .and_then(|e| e.get("type"))
            .and_then(Json::as_str)
            != Some("MouseBehavior")
        {
            return Err(mlua::Error::RuntimeError(
                "MouseBehavior must be a MouseBehavior enum item".into(),
            ));
        }
        s.properties.insert(key.into(), v);
        return Ok(true);
    }
    if key == "MouseIconEnabled" {
        let enabled = v
            .as_bool()
            .ok_or_else(|| mlua::Error::RuntimeError("MouseIconEnabled must be boolean".into()))?;
        s.properties.insert(key.into(), json!(enabled));
        return Ok(true);
    }
    if key == "MouseDeltaSensitivity" {
        let number = v.as_f64().ok_or_else(|| {
            mlua::Error::RuntimeError("MouseDeltaSensitivity must be numeric".into())
        })?;
        s.properties
            .insert(key.into(), json!((number as f32).clamp(-10.0, 10.0)));
        return Ok(true);
    }
    Ok(false)
}

fn input_method(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    mut args: MultiValue,
    method: &str,
) -> mlua::Result<MultiValue> {
    if !ensure_service(ctx, "UserInputService", id) {
        return Err(mlua::Error::RuntimeError(
            "live UserInputService receiver required".into(),
        ));
    }
    let mut out = MultiValue::new();
    match method {
        "GetLastInputType" => out.push_back(super::values::from_json(
            lua,
            &enum_json_lua(
                lua,
                "UserInputType",
                &state(lua)?.borrow().last_type.clone().if_empty("None"),
            )?,
        )?),
        "GetFocusedTextBox" => out.push_back(
            focused_textbox(ctx)
                .map(|id| world::instance(lua, ctx, &id))
                .transpose()?
                .unwrap_or(Value::Nil),
        ),
        "GetMouseLocation" => {
            let s = state(lua)?;
            let s = s.borrow();
            out.push_back(super::values::from_json(
                lua,
                &json!({"$vector2":[s.mouse[0],s.mouse[1]]}),
            )?);
        }
        "GetMouseDelta" => {
            let s = state(lua)?;
            let s = s.borrow();
            let behavior = s
                .properties
                .get("MouseBehavior")
                .and_then(|v| v.get("$enum"))
                .and_then(|v| v.get("value"))
                .and_then(Json::as_i64)
                .unwrap_or(0);
            let sensitivity = s
                .properties
                .get("MouseDeltaSensitivity")
                .and_then(Json::as_f64)
                .unwrap_or(1.) as f32;
            let scale = if behavior == 0 { 0. } else { sensitivity };
            out.push_back(super::values::from_json(
                lua,
                &json!({"$vector2":[s.frame_delta[0]*scale,s.frame_delta[1]*scale]}),
            )?);
        }
        "IsKeyDown" | "IsMouseButtonPressed" => {
            let arg = args.pop_front().unwrap_or(Value::Nil);
            let encoded = super::values::to_json(lua, &arg)?;
            let wanted = encoded
                .get("$enum")
                .and_then(|e| e.get("name"))
                .and_then(Json::as_str)
                .unwrap_or("");
            let down = state(lua)?.borrow().contacts.values().any(|c| {
                if method == "IsKeyDown" {
                    c.input_type == "Keyboard" && c.key == wanted
                } else {
                    c.input_type == wanted
                }
            });
            out.push_back(Value::Boolean(down));
        }
        "GetKeysPressed" | "GetMouseButtonsPressed" => {
            let s = state(lua)?;
            let s = s.borrow();
            let result = lua.create_table()?;
            let mut i = 1;
            for contact in s.contacts.values() {
                let matches = if method == "GetKeysPressed" {
                    contact.input_type == "Keyboard"
                } else {
                    contact.input_type.starts_with("MouseButton")
                };
                if matches {
                    let v = world::instance(lua, ctx, &contact.id)?;
                    result.raw_set(i, v)?;
                    i += 1;
                }
            }
            out.push_back(Value::Table(result));
        }
        _ => {
            return Err(mlua::Error::RuntimeError(
                "unsupported UserInputService method".into(),
            ))
        }
    }
    Ok(out)
}

trait EmptyFallback {
    fn if_empty<'a>(&'a self, fallback: &'a str) -> &'a str;
}
impl EmptyFallback for str {
    fn if_empty<'a>(&'a self, fallback: &'a str) -> &'a str {
        if self.is_empty() {
            fallback
        } else {
            self
        }
    }
}

fn method_keys(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    input_method(lua, ctx, id, args, "GetKeysPressed")
}
fn method_buttons(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    input_method(lua, ctx, id, args, "GetMouseButtonsPressed")
}
fn method_keydown(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    input_method(lua, ctx, id, args, "IsKeyDown")
}
fn method_buttondown(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    input_method(lua, ctx, id, args, "IsMouseButtonPressed")
}
fn method_last(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    input_method(lua, ctx, id, args, "GetLastInputType")
}
fn method_focused(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    input_method(lua, ctx, id, args, "GetFocusedTextBox")
}
fn method_location(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    input_method(lua, ctx, id, args, "GetMouseLocation")
}
fn method_delta(lua: &Lua, ctx: &WorldRef, id: &str, args: MultiValue) -> mlua::Result<MultiValue> {
    input_method(lua, ctx, id, args, "GetMouseDelta")
}

fn get_input(lua: &Lua, ctx: &WorldRef, id: &str, key: &str) -> mlua::Result<Option<Value>> {
    if !world::is_a(ctx, id, "InputObject")
        || !matches!(
            key,
            "KeyCode" | "UserInputType" | "UserInputState" | "Position" | "Delta"
        )
    {
        return Ok(None);
    }
    let data = world::get_property(ctx, id, key)?;
    Ok(Some(super::values::from_json(lua, &data)?))
}

fn set_input(lua: &Lua, ctx: &WorldRef, id: &str, value: Value, key: &str) -> mlua::Result<bool> {
    if !world::is_a(ctx, id, "InputObject")
        || !matches!(
            key,
            "KeyCode" | "UserInputType" | "UserInputState" | "Position" | "Delta"
        )
    {
        return Ok(false);
    }
    let encoded = super::values::to_json(lua, &value)?;
    if matches!(key, "KeyCode" | "UserInputType" | "UserInputState")
        && encoded.get("$enum").is_none()
    {
        return Err(mlua::Error::RuntimeError(format!(
            "{key} must be an EnumItem"
        )));
    }
    if matches!(key, "Position" | "Delta") && encoded.get("$vector3").is_none() {
        return Err(mlua::Error::RuntimeError(format!("{key} must be Vector3")));
    }
    world::set_property(lua, ctx, id, key, encoded)?;
    Ok(true)
}

/// Apply a frontend/local input command to this VM. The accepted event fields
/// match the existing input bridge schema; an array may be supplied directly
/// or as `{ "events": [...] }`. No OS device is touched.
pub fn action(lua: &Lua, ctx: &WorldRef, action: &Json) -> mlua::Result<()> {
    action_with_origin(lua, ctx, action, false)
}

fn action_with_origin(
    lua: &Lua,
    ctx: &WorldRef,
    action: &Json,
    synthetic: bool,
) -> mlua::Result<()> {
    if action
        .get("kind")
        .and_then(Json::as_str)
        .is_some_and(|k| matches!(k, "text_focus" | "text_blur" | "text_edit" | "text_select"))
    {
        return text_action(lua, ctx, action);
    }
    if let Some(kind) = action
        .get("kind")
        .and_then(Json::as_str)
        .filter(|k| matches!(*k, "click" | "pointer_down" | "pointer_up"))
    {
        pointer_action(lua, ctx, action)?;
        let phase = if kind == "pointer_down" {
            "Begin"
        } else {
            "End"
        };
        super::idle::record_activity(lua, "MouseButton1", phase, synthetic);
        return Ok(());
    }
    if action
        .get("kind")
        .and_then(Json::as_str)
        .is_some_and(|k| matches!(k, "scroll" | "wheel"))
    {
        scroll_action(lua, ctx, action)?;
        super::idle::record_activity(lua, "MouseWheel", "Change", synthetic);
        return Ok(());
    }
    let events = action
        .as_array()
        .or_else(|| action.get("events").and_then(Json::as_array))
        .ok_or_else(|| mlua::Error::RuntimeError("input action requires an events array".into()))?;
    if events.len() > MAX_BATCH {
        return Err(mlua::Error::RuntimeError(
            "input batch limit exceeded".into(),
        ));
    }
    let sid = ctx
        .borrow()
        .services
        .get("UserInputService")
        .cloned()
        .ok_or_else(|| mlua::Error::RuntimeError("input service unavailable".into()))?;
    let mut parsed = Vec::with_capacity(events.len());
    for e in events {
        let phase = e
            .get("phase")
            .and_then(Json::as_str)
            .ok_or_else(|| mlua::Error::RuntimeError("input event phase is missing".into()))?;
        let input_type = e
            .get("input_type")
            .and_then(Json::as_str)
            .ok_or_else(|| mlua::Error::RuntimeError("input type is missing".into()))?;
        let key = e.get("key_code").and_then(Json::as_str).unwrap_or("None");
        if !matches!(
            input_type,
            "Keyboard"
                | "MouseButton1"
                | "MouseButton2"
                | "MouseButton3"
                | "MouseWheel"
                | "MouseMovement"
                | "Touch"
                | "Focus"
        ) {
            return Err(mlua::Error::RuntimeError(
                "unsupported local input type".into(),
            ));
        }
        let event_state = match phase {
            "begin" => "Begin",
            "change" => "Change",
            "end" => "End",
            "cancel" => "Cancel",
            _ => {
                return Err(mlua::Error::RuntimeError(
                    "invalid input event phase".into(),
                ))
            }
        };
        if input_type == "Keyboard" && key == "None" {
            return Err(mlua::Error::RuntimeError(
                "keyboard input requires a KeyCode".into(),
            ));
        }
        if input_type != "Keyboard" && key != "None" {
            return Err(mlua::Error::RuntimeError(
                "KeyCode is only valid for keyboard input".into(),
            ));
        }
        if matches!(input_type, "MouseMovement" | "MouseWheel") && event_state != "Change" {
            return Err(mlua::Error::RuntimeError(
                "mouse movement and wheel require change phase".into(),
            ));
        }
        if input_type == "Touch" && event_input_id(e).is_none() {
            return Err(mlua::Error::RuntimeError(
                "touch input requires input_id".into(),
            ));
        }
        if e.get("processed").is_some_and(|v| !v.is_boolean()) {
            return Err(mlua::Error::RuntimeError(
                "processed must be boolean".into(),
            ));
        }
        for field in ["position", "delta"] {
            if let Some(v) = e.get(field) {
                check_vector(v, 3, field)?;
            }
        }
        if let Some(v) = e.get("mouse_location") {
            check_vector(v, 2, "mouse_location")?;
        }
        let cap = if input_type == "Keyboard" {
            "KeyboardEnabled"
        } else if input_type == "Touch" {
            "TouchEnabled"
        } else if input_type == "Focus" {
            ""
        } else {
            "MouseEnabled"
        };
        if !cap.is_empty()
            && event_state != "End"
            && event_state != "Cancel"
            && state(lua)?.borrow().capabilities.get(cap) != Some(&true)
        {
            return Err(mlua::Error::RuntimeError(format!("{cap} is disabled")));
        }
        if input_type == "Focus" && !matches!(event_state, "Begin" | "End" | "Cancel") {
            return Err(mlua::Error::RuntimeError(
                "window focus input requires begin or end".into(),
            ));
        }
        parsed.push((
            e.clone(),
            input_type.to_owned(),
            key.to_owned(),
            event_state.to_owned(),
        ));
    }
    for (event, input_type, key, state_name) in parsed {
        super::idle::record_activity(lua, &input_type, &state_name, synthetic);
        let key_name = if input_type == "Keyboard" {
            key.as_str()
        } else {
            "None"
        };
        let input_tag = enum_json_lua(lua, "UserInputType", &input_type)?;
        let key_tag = enum_json_lua(lua, "KeyCode", key_name)?;
        let id;
        let (old_type, old_preferred);
        {
            let state = state(lua)?;
            let mut s = state.borrow_mut();
            old_type = s.last_type.clone();
            old_preferred = s.preferred.clone();
            let identity = if input_type == "Keyboard" {
                format!("key:{key}")
            } else if let Some(token) = event_input_id(&event) {
                format!("{input_type}:{token}")
            } else {
                input_type.to_owned()
            };
            let continuous = matches!(input_type.as_str(), "MouseMovement" | "MouseWheel");
            if state_name == "Begin"
                && !s.contacts.contains_key(&identity)
                && s.contacts.len() >= MAX_HELD
            {
                return Err(mlua::Error::RuntimeError(
                    "pressed input limit exceeded".into(),
                ));
            }
            if state_name == "Begin" && s.contacts.contains_key(&identity) {
                return Err(mlua::Error::RuntimeError("input is already pressed".into()));
            }
            if !continuous && state_name != "Begin" && !s.contacts.contains_key(&identity) {
                return Err(mlua::Error::RuntimeError(
                    "input must begin before change or end".into(),
                ));
            }
            let existing = if continuous {
                s.continuous.get(&input_type).cloned()
            } else {
                s.contacts.get(&identity).map(|c| c.id.clone())
            };
            id = if let Some(id) = existing {
                id
            } else {
                s.next_id = s.next_id.saturating_add(1);
                format!("__input_{}", s.next_id)
            };
            let type_value = input_tag.clone();
            let key_value = key_tag.clone();
            let mut node = Node {
                id: id.clone(),
                class_name: "InputObject".into(),
                name: "InputObject".into(),
                parent: None,
                destroyed: false,
                server_owned: false,
                properties: Default::default(),
                local_overrides: Default::default(),
                attributes: Default::default(),
                attributes_known: true,
                revisions: Default::default(),
                local_revision: 0,
                collection_tags: Default::default(),
                collection_tags_known: true,
                projected: None,
            };
            node.properties.insert("UserInputType".into(), type_value);
            node.properties.insert("KeyCode".into(), key_value);
            node.properties.insert(
                "UserInputState".into(),
                enum_json_lua(lua, "UserInputState", &state_name)?,
            );
            node.properties.insert(
                "Position".into(),
                event
                    .get("position")
                    .cloned()
                    .map(|a| json!({"$vector3":a}))
                    .unwrap_or(json!({"$vector3":[0,0,0]})),
            );
            node.properties.insert(
                "Delta".into(),
                event
                    .get("delta")
                    .cloned()
                    .map(|a| json!({"$vector3":a}))
                    .unwrap_or(json!({"$vector3":[0,0,0]})),
            );
            ctx.borrow_mut().nodes.insert(id.clone(), node);
            if continuous {
                s.continuous.insert(input_type.clone(), id.clone());
            } else if state_name == "End" || state_name == "Cancel" {
                s.contacts.remove(&identity);
            } else {
                s.contacts.insert(
                    identity,
                    Contact {
                        id: id.clone(),
                        input_type: input_type.clone(),
                        key: key.clone(),
                    },
                );
            }
            s.last_type = input_type.clone();
            if state_name != "End"
                && state_name != "Cancel"
                && matches!(
                    input_type.as_str(),
                    "Keyboard"
                        | "MouseButton1"
                        | "MouseButton2"
                        | "MouseButton3"
                        | "MouseWheel"
                        | "MouseMovement"
                )
                && (s.capabilities.get("KeyboardEnabled") == Some(&true)
                    || s.capabilities.get("MouseEnabled") == Some(&true))
            {
                s.preferred = "KeyboardAndMouse".into();
            }
            if state_name != "End"
                && state_name != "Cancel"
                && matches!(
                    input_type.as_str(),
                    "Keyboard"
                        | "MouseButton1"
                        | "MouseButton2"
                        | "MouseButton3"
                        | "MouseWheel"
                        | "MouseMovement"
                )
                && (s.capabilities.get("KeyboardEnabled") == Some(&true)
                    || s.capabilities.get("MouseEnabled") == Some(&true))
            {
                s.preferred = "KeyboardAndMouse".into();
            }
            if let Some(loc) = event
                .get("mouse_location")
                .and_then(Json::as_array)
                .filter(|a| a.len() == 2)
            {
                s.mouse = [
                    loc[0].as_f64().unwrap_or(0.) as f32,
                    loc[1].as_f64().unwrap_or(0.) as f32,
                ];
            }
            if let Some(delta) = event
                .get("delta")
                .and_then(Json::as_array)
                .filter(|a| a.len() >= 2)
            {
                s.frame_delta = [
                    delta[0].as_f64().unwrap_or(0.) as f32,
                    delta[1].as_f64().unwrap_or(0.) as f32,
                ];
            }
        }
        let mut args = MultiValue::new();
        args.push_back(world::instance(lua, ctx, &id)?);
        args.push_back(Value::Boolean(
            event
                .get("processed")
                .and_then(Json::as_bool)
                .unwrap_or(false),
        ));
        if old_preferred != state(lua)?.borrow().preferred {
            world::emit_property_changed(lua, ctx, &sid, "PreferredInput")?;
        }
        if old_type != input_type {
            let item = super::enums::enum_item(lua, "UserInputType", &input_type)?;
            super::signals::fire(
                lua,
                ctx,
                &sid,
                "LastInputTypeChanged",
                MultiValue::from_vec(vec![Value::UserData(item)]),
            )?;
        }
        let signal = match state_name.as_str() {
            "Begin" => "InputBegan",
            "Change" => "InputChanged",
            _ => "InputEnded",
        };
        super::signals::fire(lua, ctx, &sid, signal, args)?;
        if input_type == "Focus" && state_name == "Begin" {
            super::signals::fire(lua, ctx, &sid, "WindowFocused", MultiValue::new())?;
        }
        if input_type == "Focus" && matches!(state_name.as_str(), "End" | "Cancel") {
            super::signals::fire(lua, ctx, &sid, "WindowFocusReleased", MultiValue::new())?;
        }
        if input_type == "Touch" {
            let touch_signal = match state_name.as_str() {
                "Begin" => "TouchStarted",
                "Change" => "TouchMoved",
                _ => "TouchEnded",
            };
            let touch_args = MultiValue::from_vec(vec![
                world::instance(lua, ctx, &id)?,
                Value::Boolean(
                    event
                        .get("processed")
                        .and_then(Json::as_bool)
                        .unwrap_or(false),
                ),
            ]);
            super::signals::fire(lua, ctx, &sid, touch_signal, touch_args)?;
        }
        dispatch_context_actions(
            lua,
            ctx,
            &sid,
            &id,
            &input_type,
            &key,
            &state_name,
            event
                .get("processed")
                .and_then(Json::as_bool)
                .unwrap_or(false),
        )?;
    }
    record_input_events(lua, events)?;
    Ok(())
}

/// Read the currently focused local TextBox from the per-session world.
pub fn focused_textbox(ctx: &WorldRef) -> Option<String> {
    ctx.borrow().focused_textbox.clone()
}

fn text_action(lua: &Lua, ctx: &WorldRef, action: &Json) -> mlua::Result<()> {
    let obj = action
        .as_object()
        .ok_or_else(|| mlua::Error::RuntimeError("Invalid TextBox interaction".into()))?;
    let kind = obj.get("kind").and_then(Json::as_str).unwrap_or("");
    let id = obj
        .get("id")
        .and_then(Json::as_str)
        .ok_or_else(|| mlua::Error::RuntimeError("TextBox target required".into()))?;
    if kind == "text_focus" {
        if obj.len() != 2 || !world::is_a(ctx, id, "TextBox") {
            return Err(mlua::Error::RuntimeError("Invalid TextBox focus".into()));
        }
        if !gui::is_visible(ctx, id)
            || world::get_property(ctx, id, "Interactable")
                .ok()
                .and_then(|v| v.as_bool())
                == Some(false)
        {
            return Ok(());
        }
        let previous = ctx.borrow().focused_textbox.clone();
        if previous.as_deref() == Some(id) {
            return Ok(());
        }
        if let Some(old) = previous {
            release_textbox(lua, ctx, &old, false)?;
        }
        let mut text = world::get_property(ctx, id, "Text")
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default();
        if world::get_property(ctx, id, "ClearTextOnFocus")
            .ok()
            .and_then(|v| v.as_bool())
            == Some(true)
            && world::get_property(ctx, id, "TextEditable")
                .ok()
                .and_then(|v| v.as_bool())
                != Some(false)
        {
            if !text.is_empty() {
                world::set_property(lua, ctx, id, "Text", json!(""))?;
                text.clear();
            }
        }
        {
            let mut w = ctx.borrow_mut();
            w.focused_textbox = Some(id.to_owned());
            w.focused_text = text.clone();
            w.focused_text_revision = w.focused_text_revision.wrapping_add(1);
            w.focused_editor_revision = 0;
            w.focused_cursor = text.len().saturating_add(1).min(i32::MAX as usize) as i32;
            w.focused_anchor = -1;
        }
        super::signals::fire(lua, ctx, id, "Focused", MultiValue::new())?;
        fire_service_text_event(lua, ctx, "TextBoxFocused", id)?;
        return Ok(());
    }
    let revision = obj
        .get("revision")
        .and_then(Json::as_u64)
        .ok_or_else(|| mlua::Error::RuntimeError("Invalid TextBox revision".into()))?;
    let current = {
        let w = ctx.borrow();
        w.focused_textbox.as_deref() == Some(id) && w.focused_text_revision == revision
    };
    if !current {
        return Ok(());
    }
    if !gui::is_visible(ctx, id) {
        release_textbox(lua, ctx, id, false)?;
        return Ok(());
    }
    if kind == "text_blur" {
        if obj.len() != 4 {
            return Err(mlua::Error::RuntimeError(
                "Invalid TextBox submission".into(),
            ));
        }
        let submitted = obj
            .get("submitted")
            .and_then(Json::as_bool)
            .ok_or_else(|| mlua::Error::RuntimeError("Invalid TextBox submission".into()))?;
        release_textbox(lua, ctx, id, submitted)?;
        return Ok(());
    }
    let edit = kind == "text_edit";
    let selection = kind == "text_select" || (edit && obj.contains_key("editorRevision"));
    if !(kind == "text_select" || edit)
        || obj.len()
            != (if selection {
                if edit {
                    7
                } else {
                    6
                }
            } else {
                4
            })
    {
        return Err(mlua::Error::RuntimeError(
            "Invalid TextBox interaction".into(),
        ));
    }
    let (mut cursor, anchor, editor_revision) = if selection {
        let e = obj
            .get("editorRevision")
            .and_then(Json::as_u64)
            .ok_or_else(|| mlua::Error::RuntimeError("Invalid TextBox selection".into()))?;
        if e != ctx.borrow().focused_editor_revision {
            return Ok(());
        }
        let c = obj
            .get("cursor")
            .and_then(Json::as_u64)
            .filter(|v| (1..=4097).contains(v))
            .ok_or_else(|| mlua::Error::RuntimeError("Invalid TextBox selection".into()))?;
        let a = obj
            .get("anchor")
            .and_then(Json::as_i64)
            .filter(|v| *v >= -1 && *v != 0 && *v <= 4097)
            .ok_or_else(|| mlua::Error::RuntimeError("Invalid TextBox selection".into()))?;
        (c as i32, a as i32, e)
    } else {
        (1, -1, ctx.borrow().focused_editor_revision)
    };
    if edit {
        let text = obj
            .get("text")
            .and_then(Json::as_str)
            .filter(|s| s.len() <= 4096)
            .ok_or_else(|| mlua::Error::RuntimeError("Invalid TextBox text".into()))?
            .to_owned();
        if world::get_property(ctx, id, "TextEditable")
            .ok()
            .and_then(|v| v.as_bool())
            == Some(false)
        {
            return Err(mlua::Error::RuntimeError("TextBox is read-only".into()));
        }
        world::set_property(lua, ctx, id, "Text", json!(text))?;
        let mut w = ctx.borrow_mut();
        w.focused_text = text.clone();
        w.focused_editor_revision = w.focused_editor_revision.wrapping_add(1);
        if !selection {
            cursor = text.len().saturating_add(1).min(i32::MAX as usize) as i32;
        }
    }
    let mut w = ctx.borrow_mut();
    w.focused_cursor = cursor;
    w.focused_anchor = anchor;
    let _ = editor_revision;
    Ok(())
}

fn release_textbox(lua: &Lua, ctx: &WorldRef, id: &str, submitted: bool) -> mlua::Result<()> {
    let should_release = {
        let w = ctx.borrow();
        w.focused_textbox.as_deref() == Some(id)
    };
    if !should_release {
        return Ok(());
    }
    {
        let mut w = ctx.borrow_mut();
        w.focused_textbox = None;
        w.focused_text_revision = w.focused_text_revision.wrapping_add(1);
        w.focused_cursor = -1;
        w.focused_anchor = -1;
    }
    let mut args = MultiValue::new();
    args.push_back(Value::Boolean(submitted));
    if submitted {
        let input = write_text_submit_input(ctx)?;
        args.push_back(world::instance(lua, ctx, &input)?);
    } else {
        args.push_back(Value::Nil);
    }
    super::signals::fire(lua, ctx, id, "FocusLost", args)?;
    fire_service_text_event(lua, ctx, "TextBoxFocusReleased", id)
}

fn fire_service_text_event(lua: &Lua, ctx: &WorldRef, key: &str, id: &str) -> mlua::Result<()> {
    let sid = ctx.borrow().services.get("UserInputService").cloned();
    if let Some(sid) = sid {
        let mut args = MultiValue::new();
        args.push_back(world::instance(lua, ctx, id)?);
        super::signals::fire(lua, ctx, &sid, key, args)?;
    }
    Ok(())
}

fn write_text_submit_input(ctx: &WorldRef) -> mlua::Result<String> {
    const ID: &str = "__local_text_submit";
    let props = json!({"UserInputType":{"$enum":{"type":"UserInputType","name":"Keyboard","value":8}},
        "KeyCode":{"$enum":{"type":"KeyCode","name":"Return","value":13}},
        "UserInputState":{"$enum":{"type":"UserInputState","name":"Begin","value":0}},
        "Position":{"$vector3":[0,0,0]},"Delta":{"$vector3":[0,0,0]}});
    let mut world = ctx.borrow_mut();
    if let Some(node) = world.nodes.get_mut(ID) {
        node.properties = props.as_object().cloned().unwrap_or_default();
    } else {
        world.nodes.insert(
            ID.into(),
            Node {
                id: ID.into(),
                class_name: "InputObject".into(),
                name: "InputObject".into(),
                parent: None,
                destroyed: false,
                server_owned: false,
                properties: props.as_object().cloned().unwrap_or_default(),
                local_overrides: Default::default(),
                attributes: Default::default(),
                attributes_known: true,
                revisions: Default::default(),
                local_revision: 0,
                collection_tags: Default::default(),
                collection_tags_known: true,
                projected: None,
            },
        );
    }
    Ok(ID.into())
}

fn event_input_id(event: &Json) -> Option<&str> {
    event
        .get("input_id")
        .and_then(Json::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 64)
}
fn check_vector(value: &Json, n: usize, name: &str) -> mlua::Result<()> {
    let values = value
        .as_array()
        .filter(|a| a.len() == n)
        .ok_or_else(|| mlua::Error::RuntimeError(format!("{name} requires {n} components")))?;
    if values.iter().any(|v| {
        v.as_f64()
            .is_none_or(|x| !x.is_finite() || x.abs() > f32::MAX as f64)
    }) {
        return Err(mlua::Error::RuntimeError(format!(
            "{name} values must be finite"
        )));
    }
    Ok(())
}

fn pointer_action(lua: &Lua, ctx: &WorldRef, payload: &Json) -> mlua::Result<()> {
    let kind = payload.get("kind").and_then(Json::as_str).unwrap_or("");
    let id = payload
        .get("id")
        .and_then(Json::as_str)
        .ok_or_else(|| mlua::Error::RuntimeError("GUI target required".into()))?;
    if payload.as_object().is_none_or(|o| o.len() != 2) || !world::is_a(ctx, id, "GuiObject") {
        return Err(mlua::Error::RuntimeError(
            "invalid local GUI pointer action".into(),
        ));
    }
    if kind == "click" {
        if !world::is_a(ctx, id, "GuiButton") || !gui::is_visible(ctx, id) {
            return Err(mlua::Error::RuntimeError(
                "GUI target is not an interactable button".into(),
            ));
        }
        if world::get_property(ctx, id, "Interactable")
            .ok()
            .and_then(|v| v.as_bool())
            == Some(false)
        {
            return Err(mlua::Error::RuntimeError("GUI target is disabled".into()));
        }
        let bounds = gui::layout_bounds(ctx, id).map_err(mlua::Error::RuntimeError)?;
        let (x, y) = (bounds[0] + bounds[2] * 0.5, bounds[1] + bounds[3] * 0.5);
        let input = write_pointer_input(ctx, "End", x, y)?;
        super::signals::fire(
            lua,
            ctx,
            id,
            "MouseButton1Down",
            MultiValue::from_vec(vec![Value::Number(x), Value::Number(y)]),
        )?;
        super::signals::fire(
            lua,
            ctx,
            id,
            "MouseButton1Up",
            MultiValue::from_vec(vec![Value::Number(x), Value::Number(y)]),
        )?;
        super::signals::fire(lua, ctx, id, "MouseButton1Click", MultiValue::new())?;
        super::signals::fire(
            lua,
            ctx,
            id,
            "Activated",
            MultiValue::from_vec(vec![world::instance(lua, ctx, &input)?, Value::Integer(1)]),
        )?;
        let service = ctx
            .borrow()
            .services
            .get("UserInputService")
            .cloned()
            .unwrap_or_default();
        dispatch_context_actions(
            lua,
            ctx,
            &service,
            &input,
            "MouseButton1",
            "None",
            "End",
            false,
        )?;
        return Ok(());
    }
    let down = kind == "pointer_down";
    if kind != "pointer_down" && kind != "pointer_up" {
        return Err(mlua::Error::RuntimeError(
            "unsupported GUI pointer action".into(),
        ));
    }
    let current = {
        let state = state(lua)?;
        let state = state.borrow();
        state.gui_pointer_target.clone()
    };
    if down {
        if !gui::is_visible(ctx, id)
            || current.is_some()
            || world::get_property(ctx, id, "Interactable")
                .ok()
                .and_then(|v| v.as_bool())
                == Some(false)
        {
            return Err(mlua::Error::RuntimeError(
                "GUI target is unavailable for pointer input".into(),
            ));
        }
    } else if current.as_deref() != Some(id) {
        return Err(mlua::Error::RuntimeError(
            "GUI pointer must begin on this target".into(),
        ));
    }
    let (x, y) = if down {
        let bounds = gui::layout_bounds(ctx, id).map_err(mlua::Error::RuntimeError)?;
        let pos = [bounds[0] + bounds[2] * 0.5, bounds[1] + bounds[3] * 0.5];
        let state = state(lua)?;
        let mut state = state.borrow_mut();
        state.gui_pointer_target = Some(id.to_owned());
        state.gui_pointer_position = pos;
        (pos[0], pos[1])
    } else {
        let state = state(lua)?;
        let state = state.borrow();
        (state.gui_pointer_position[0], state.gui_pointer_position[1])
    };
    if down {
        if world::is_a(ctx, id, "TextBox") {
            text_action(lua, ctx, &json!({"kind":"text_focus","id":id}))?;
        } else if let Some(focused) = focused_textbox(ctx) {
            release_textbox(lua, ctx, &focused, false)?;
        }
    }
    let input = write_pointer_input(ctx, if down { "Begin" } else { "End" }, x, y)?;
    super::signals::fire(
        lua,
        ctx,
        id,
        if down { "InputBegan" } else { "InputEnded" },
        MultiValue::from_vec(vec![world::instance(lua, ctx, &input)?]),
    )?;
    if world::is_a(ctx, id, "GuiButton") {
        super::signals::fire(
            lua,
            ctx,
            id,
            if down {
                "MouseButton1Down"
            } else {
                "MouseButton1Up"
            },
            MultiValue::from_vec(vec![Value::Number(x), Value::Number(y)]),
        )?;
        if !down
            && gui::is_visible(ctx, id)
            && world::get_property(ctx, id, "Interactable")
                .ok()
                .and_then(|v| v.as_bool())
                != Some(false)
        {
            super::signals::fire(lua, ctx, id, "MouseButton1Click", MultiValue::new())?;
            super::signals::fire(
                lua,
                ctx,
                id,
                "Activated",
                MultiValue::from_vec(vec![world::instance(lua, ctx, &input)?, Value::Integer(1)]),
            )?;
        }
    }
    let service = ctx
        .borrow()
        .services
        .get("UserInputService")
        .cloned()
        .unwrap_or_default();
    dispatch_context_actions(
        lua,
        ctx,
        &service,
        &input,
        "MouseButton1",
        "None",
        if down { "Begin" } else { "End" },
        false,
    )?;
    if !down {
        let state = state(lua)?;
        state.borrow_mut().gui_pointer_target = None;
    }
    Ok(())
}

fn write_pointer_input(ctx: &WorldRef, state: &str, x: f64, y: f64) -> mlua::Result<String> {
    const ID: &str = "__local_gui_pointer";
    let mut world = ctx.borrow_mut();
    let props = json!({
        "UserInputType":{"$enum":{"type":"UserInputType","name":"MouseButton1","value":0}},
        "KeyCode":{"$enum":{"type":"KeyCode","name":"None","value":0}},
        "UserInputState":{"$enum":{"type":"UserInputState","name":state,"value":match state{"Begin"=>0,"End"=>2,_=>3}}},
        "Position":{"$vector3":[x,y,0]},"Delta":{"$vector3":[0,0,0]}
    });
    if let Some(node) = world.nodes.get_mut(ID) {
        node.properties = props.as_object().cloned().unwrap_or_default();
    } else {
        let node = Node {
            id: ID.into(),
            class_name: "InputObject".into(),
            name: "InputObject".into(),
            parent: None,
            destroyed: false,
            server_owned: false,
            properties: props.as_object().cloned().unwrap_or_default(),
            local_overrides: Default::default(),
            attributes: Default::default(),
            attributes_known: true,
            revisions: Default::default(),
            local_revision: 0,
            collection_tags: Default::default(),
            collection_tags_known: true,
            projected: None,
        };
        world.nodes.insert(ID.into(), node);
    }
    Ok(ID.into())
}

fn scroll_action(lua: &Lua, ctx: &WorldRef, payload: &Json) -> mlua::Result<()> {
    let kind = payload.get("kind").and_then(Json::as_str).unwrap_or("");
    let id = payload
        .get("id")
        .and_then(Json::as_str)
        .ok_or_else(|| mlua::Error::RuntimeError("GUI target required".into()))?;
    let wheel = kind == "wheel";
    if !wheel && kind != "scroll" {
        return Err(mlua::Error::RuntimeError(
            "unsupported scroll action".into(),
        ));
    }
    if payload
        .as_object()
        .is_none_or(|o| o.len() != if wheel { 5 } else { 3 })
    {
        return Err(mlua::Error::RuntimeError(
            "invalid scroll interaction".into(),
        ));
    }
    if !gui::is_visible(ctx, id) || !world::is_a(ctx, id, "GuiObject") {
        return Err(mlua::Error::RuntimeError(
            "GUI target is no longer visible".into(),
        ));
    }
    let delta = payload
        .get("delta")
        .and_then(Json::as_array)
        .filter(|a| a.len() == 2)
        .ok_or_else(|| mlua::Error::RuntimeError("scroll requires two pixel deltas".into()))?;
    let mut dx = bounded_number(&delta[0], 4096.0, "scroll delta")?;
    let mut dy = bounded_number(&delta[1], 4096.0, "scroll delta")?;
    let (x, y, steps) = if wheel {
        let p = payload
            .get("position")
            .and_then(Json::as_array)
            .filter(|a| a.len() == 2)
            .ok_or_else(|| mlua::Error::RuntimeError("wheel requires a screen position".into()))?;
        let steps = payload
            .get("steps")
            .and_then(Json::as_i64)
            .filter(|v| (-128..=128).contains(v))
            .ok_or_else(|| {
                mlua::Error::RuntimeError("wheel steps must be an integer within bounds".into())
            })?;
        (
            bounded_number(&p[0], 32768.0, "wheel position")?,
            bounded_number(&p[1], 32768.0, "wheel position")?,
            steps as i32,
        )
    } else {
        (0., 0., 0)
    };
    let mut current = Some(id.to_owned());
    let mut consumed = false;
    for _ in 0..128 {
        let Some(target) = current.clone() else { break };
        if world::is_a(ctx, &target, "ScrollingFrame") {
            let used = gui::scroll_by(ctx, &target, [dx, dy]).map_err(mlua::Error::RuntimeError)?;
            dx = (dx - used[0]).clamp(-4096., 4096.);
            dy = (dy - used[1]).clamp(-4096., 4096.);
            consumed |= used[0].abs() > 1e-8 || used[1].abs() > 1e-8;
            if dx.abs() < 1e-8 && dy.abs() < 1e-8 {
                break;
            }
        }
        current = ctx
            .borrow()
            .nodes
            .get(&target)
            .and_then(|n| n.parent.clone());
        if current
            .as_deref()
            .is_some_and(|p| world::is_a(ctx, p, "ScreenGui"))
        {
            break;
        }
    }
    if !wheel {
        return Ok(());
    }
    let input_delta = [
        dx,
        if steps != 0 { -40.0 * steps as f64 } else { dy },
        steps as f64,
    ];
    let event = json!({"phase":"change","input_type":"MouseWheel","position":[x,y,steps],"delta":input_delta,"mouse_location":[x,y],"processed":consumed});
    with_synthetic_capabilities(lua, || action(lua, ctx, &json!({"events":[event]})))?;
    let input = {
        let state = state(lua)?;
        let state = state.borrow();
        state.continuous.get("MouseWheel").cloned()
    };
    if let Some(input) = input {
        let object = world::instance(lua, ctx, &input)?;
        let mut current = Some(id.to_owned());
        for _ in 0..128 {
            let Some(target) = current.clone() else { break };
            if world::is_a(ctx, &target, "GuiObject") && gui::is_visible(ctx, &target) {
                super::signals::fire(
                    lua,
                    ctx,
                    &target,
                    "InputChanged",
                    MultiValue::from_vec(vec![object.clone()]),
                )?;
                if steps != 0 {
                    super::signals::fire(
                        lua,
                        ctx,
                        &target,
                        if steps > 0 {
                            "MouseWheelForward"
                        } else {
                            "MouseWheelBackward"
                        },
                        MultiValue::from_vec(vec![Value::Number(x), Value::Number(y)]),
                    )?;
                }
            }
            current = ctx
                .borrow()
                .nodes
                .get(&target)
                .and_then(|n| n.parent.clone());
        }
    }
    Ok(())
}

fn bounded_number(value: &Json, bound: f64, name: &str) -> mlua::Result<f64> {
    let n = value
        .as_f64()
        .filter(|n| n.is_finite() && n.abs() <= bound)
        .ok_or_else(|| {
            mlua::Error::RuntimeError(format!("{name} must be finite and within bounds"))
        })?;
    Ok(n)
}
fn with_synthetic_capabilities<T>(
    lua: &Lua,
    run: impl FnOnce() -> mlua::Result<T>,
) -> mlua::Result<T> {
    let reference = state(lua)?;
    let (caps, preferred) = {
        let mut s = reference.borrow_mut();
        let caps = s.capabilities.clone();
        let preferred = s.preferred.clone();
        for key in ["KeyboardEnabled", "MouseEnabled", "TouchEnabled"] {
            s.capabilities.insert(key.into(), true);
        }
        (caps, preferred)
    };
    let result = run();
    let mut s = reference.borrow_mut();
    s.capabilities = caps;
    s.preferred = preferred;
    drop(s);
    result
}

fn context_action_method(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
    method: &str,
) -> mlua::Result<MultiValue> {
    if !world::is_a(ctx, id, "ContextActionService") {
        return Err(mlua::Error::RuntimeError(
            "live ContextActionService receiver required".into(),
        ));
    }
    let mut args: Vec<Value> = args.into_iter().collect();
    let name = match args.first() {
        Some(Value::String(s)) => s.to_str()?.to_string(),
        _ => {
            return Err(mlua::Error::RuntimeError(
                "context action name must be a string".into(),
            ))
        }
    };
    if name.len() > 256 {
        return Err(mlua::Error::RuntimeError(
            "context action name exceeds 256 bytes".into(),
        ));
    }
    let mut result = MultiValue::new();
    match method {
        "UnbindAction" => {
            let state = state(lua)?;
            state.borrow_mut().context_actions.remove(&name);
        }
        "UnbindAllActions" => {
            let state = state(lua)?;
            state.borrow_mut().context_actions.clear();
        }
        "GetBoundActionInfo" => {
            let state = state(lua)?;
            let state = state.borrow();
            let table = lua.create_table()?;
            if let Some(binding) = state.context_actions.get(&name) {
                table.set("priorityLevel", binding.priority)?;
                let inputs = lua.create_table()?;
                for (i, (kind, item)) in binding.inputs.iter().enumerate() {
                    inputs.set(i + 1, super::enums::enum_item(lua, kind, item)?)?;
                }
                table.set("inputTypes", inputs)?;
                table.set("createTouchButton", false)?;
            }
            result.push_back(Value::Table(table));
        }
        "GetAllBoundActionInfo" => {
            let state = state(lua)?;
            let state = state.borrow();
            let table = lua.create_table()?;
            for (name, binding) in &state.context_actions {
                let info = lua.create_table()?;
                info.set("priorityLevel", binding.priority)?;
                let inputs = lua.create_table()?;
                for (i, (kind, item)) in binding.inputs.iter().enumerate() {
                    inputs.set(i + 1, super::enums::enum_item(lua, kind, item)?)?;
                }
                info.set("inputTypes", inputs)?;
                info.set("createTouchButton", false)?;
                table.set(name.as_str(), info)?;
            }
            result.push_back(Value::Table(table));
        }
        "BindAction" | "BindActionAtPriority" | "BindActionToInputTypes" => {
            if args.len() < 3 {
                return Err(mlua::Error::RuntimeError(
                    "BindAction expects a name, callback and touch flag".into(),
                ));
            }
            let callback = match args.get(1) {
                Some(Value::Function(f)) => f.clone(),
                _ => {
                    return Err(mlua::Error::RuntimeError(
                        "context action callback must be a function".into(),
                    ))
                }
            };
            let touch = match args.get(2) {
                Some(Value::Boolean(v)) => *v,
                _ => {
                    return Err(mlua::Error::RuntimeError(
                        "createTouchButton must be boolean".into(),
                    ))
                }
            };
            let (priority, first) = if method == "BindActionAtPriority" {
                let p = args
                    .get(3)
                    .and_then(|v| match v {
                        Value::Integer(n) => Some(*n),
                        Value::Number(n) if n.is_finite() && n.fract() == 0. => Some(*n as i64),
                        _ => None,
                    })
                    .ok_or_else(|| {
                        mlua::Error::RuntimeError("priorityLevel must be a signed integer".into())
                    })?;
                if p < i32::MIN as i64 || p > i32::MAX as i64 {
                    return Err(mlua::Error::RuntimeError(
                        "priorityLevel must fit signed 32-bit integer".into(),
                    ));
                }
                (p as i32, 4)
            } else {
                (2000, 3)
            };
            if args.len().saturating_sub(first) > 64 {
                return Err(mlua::Error::RuntimeError(
                    "context action accepts at most 64 input types".into(),
                ));
            }
            let mut inputs = Vec::new();
            for value in &args[first..] {
                let encoded = super::values::to_json(lua, value)?;
                if let Some(e) = encoded.get("$enum") {
                    let kind = e.get("type").and_then(Json::as_str).unwrap_or("");
                    let item = e.get("name").and_then(Json::as_str).unwrap_or("");
                    if !matches!(kind, "KeyCode" | "UserInputType" | "PlayerActions") {
                        return Err(mlua::Error::RuntimeError(
                            "context hotkey has incorrect enum type".into(),
                        ));
                    }
                    inputs.push((kind.to_owned(), item.to_owned()));
                } else if let Value::String(s) = value {
                    let text = s.to_str()?;
                    if text.len() > 256 {
                        return Err(mlua::Error::RuntimeError(
                            "context hotkey string is too long".into(),
                        ));
                    }
                    inputs.push(("string".into(), text.to_string()));
                } else {
                    return Err(mlua::Error::RuntimeError(
                        "context hotkey must be an input enum item or string".into(),
                    ));
                }
            }
            let state = state(lua)?;
            let mut state = state.borrow_mut();
            if !state.context_actions.contains_key(&name) && state.context_actions.len() >= 256 {
                return Err(mlua::Error::RuntimeError(
                    "context action binding limit exceeded".into(),
                ));
            }
            let sequence = state.next_context_sequence;
            state.next_context_sequence = sequence.wrapping_add(1);
            state.context_actions.insert(
                name,
                ContextBinding {
                    callback,
                    priority,
                    inputs,
                    sequence,
                },
            );
            let _ = touch;
        }
        "SetDescription" | "SetTitle" | "SetImage" | "SetPosition" | "GetButton" => {
            if method == "GetButton" {
                result.push_back(Value::Nil);
            }
        }
        _ => {
            return Err(mlua::Error::RuntimeError(
                "unsupported ContextActionService method".into(),
            ))
        }
    }
    Ok(result)
}

fn cas_bind(lua: &Lua, c: &WorldRef, i: &str, a: MultiValue) -> mlua::Result<MultiValue> {
    context_action_method(lua, c, i, a, "BindAction")
}
fn cas_bind_priority(lua: &Lua, c: &WorldRef, i: &str, a: MultiValue) -> mlua::Result<MultiValue> {
    context_action_method(lua, c, i, a, "BindActionAtPriority")
}
fn cas_bind_types(lua: &Lua, c: &WorldRef, i: &str, a: MultiValue) -> mlua::Result<MultiValue> {
    context_action_method(lua, c, i, a, "BindActionToInputTypes")
}
fn cas_unbind(lua: &Lua, c: &WorldRef, i: &str, a: MultiValue) -> mlua::Result<MultiValue> {
    context_action_method(lua, c, i, a, "UnbindAction")
}
fn cas_unbind_all(lua: &Lua, c: &WorldRef, i: &str, a: MultiValue) -> mlua::Result<MultiValue> {
    context_action_method(lua, c, i, a, "UnbindAllActions")
}
fn cas_info(lua: &Lua, c: &WorldRef, i: &str, a: MultiValue) -> mlua::Result<MultiValue> {
    context_action_method(lua, c, i, a, "GetBoundActionInfo")
}
fn cas_all_info(lua: &Lua, c: &WorldRef, i: &str, a: MultiValue) -> mlua::Result<MultiValue> {
    context_action_method(lua, c, i, a, "GetAllBoundActionInfo")
}
fn cas_button(lua: &Lua, c: &WorldRef, i: &str, a: MultiValue) -> mlua::Result<MultiValue> {
    context_action_method(lua, c, i, a, "GetButton")
}
fn cas_description(lua: &Lua, c: &WorldRef, i: &str, a: MultiValue) -> mlua::Result<MultiValue> {
    context_action_method(lua, c, i, a, "SetDescription")
}
fn cas_title(lua: &Lua, c: &WorldRef, i: &str, a: MultiValue) -> mlua::Result<MultiValue> {
    context_action_method(lua, c, i, a, "SetTitle")
}
fn cas_image(lua: &Lua, c: &WorldRef, i: &str, a: MultiValue) -> mlua::Result<MultiValue> {
    context_action_method(lua, c, i, a, "SetImage")
}
fn cas_position(lua: &Lua, c: &WorldRef, i: &str, a: MultiValue) -> mlua::Result<MultiValue> {
    context_action_method(lua, c, i, a, "SetPosition")
}

fn recording_document(lua: &Lua) -> mlua::Result<Json> {
    let state = state(lua)?;
    let state = state.borrow();
    let duration = state
        .recording_started
        .map(|start| start.elapsed().as_secs_f64())
        .unwrap_or(state.recording_duration);
    Ok(
        json!({"format":"rove.virtual-input","version":1,"duration":duration,"additionalLuaState":state.additional_lua_state,
        "initialMousePosition":state.recording_initial_mouse,"events":state.recording_events}),
    )
}
fn record_input_events(lua: &Lua, events: &[Json]) -> mlua::Result<()> {
    let state = state(lua)?;
    let mut state = state.borrow_mut();
    let Some(started) = state.recording_started else {
        return Ok(());
    };
    if state.recording_events.len().saturating_add(events.len()) > 10_000 {
        state.recording_last_error = "virtual input recording event limit exceeded".into();
        state.recording_duration = started.elapsed().as_secs_f64();
        state.recording_started = None;
        return Ok(());
    }
    for event in events {
        let entry =
            json!({"time":started.elapsed().as_secs_f64(),"method":"__host","args":[event]});
        let size = entry.to_string().len();
        if size > 2 * 1024 * 1024usize - state.recording_bytes {
            state.recording_last_error = "virtual input recording byte limit exceeded".into();
            state.recording_duration = started.elapsed().as_secs_f64();
            state.recording_started = None;
            return Ok(());
        }
        state.recording_bytes += size;
        state.recording_events.push(entry);
    }
    Ok(())
}

const MAX_TAPE_BYTES: usize = 2 * 1024 * 1024;
const MAX_TAPE_EVENTS: usize = 10_000;
const MAX_PLAYBACK_PER_STEP: usize = 128;
const VIM_METHODS: [&str; 7] = [
    "SendKeyEvent",
    "SendMouseButtonEvent",
    "SendMouseMoveEvent",
    "SendMouseMoveDeltaEvent",
    "SendMouseWheelEvent",
    "SendTouchEvent",
    "SendTextInputCharacterEvent",
];

fn decode_tape_value(lua: &Lua, ctx: &WorldRef, value: &Json) -> mlua::Result<Value> {
    if let Some(reference) = value.get("$ref").and_then(Json::as_str) {
        return world::instance(lua, ctx, reference);
    }
    if let Some(tag) = value.get("$enum") {
        let kind = tag
            .get("type")
            .and_then(Json::as_str)
            .ok_or_else(|| mlua::Error::RuntimeError("recording enum has no type".into()))?;
        let name = tag
            .get("name")
            .and_then(Json::as_str)
            .ok_or_else(|| mlua::Error::RuntimeError("recording enum has no name".into()))?;
        return Ok(Value::UserData(super::enums::enum_item(lua, kind, name)?));
    }
    super::values::from_json(lua, value)
}

fn validate_tape(
    lua: &Lua,
    ctx: &WorldRef,
    content: &str,
) -> mlua::Result<(Vec<Json>, f64, String, [f32; 2])> {
    if content.len() > MAX_TAPE_BYTES {
        return Err(mlua::Error::RuntimeError("recording exceeds 2 MiB".into()));
    }
    let doc: Json = serde_json::from_str(content)
        .map_err(|e| mlua::Error::RuntimeError(format!("invalid recording JSON: {e}")))?;
    if doc.get("format").and_then(Json::as_str) != Some("rove.virtual-input")
        || doc.get("version").and_then(Json::as_u64) != Some(1)
    {
        return Err(mlua::Error::RuntimeError(
            "expected a rove.virtual-input version 1 recording".into(),
        ));
    }
    let duration = doc
        .get("duration")
        .and_then(Json::as_f64)
        .filter(|n| n.is_finite() && (0.0..=3600.0).contains(n))
        .ok_or_else(|| mlua::Error::RuntimeError("recording duration is invalid".into()))?;
    let additional = doc
        .get("additionalLuaState")
        .and_then(Json::as_str)
        .filter(|s| s.len() <= 65536)
        .ok_or_else(|| mlua::Error::RuntimeError("recording additionalLuaState is invalid".into()))?
        .to_owned();
    let mut initial_mouse = [0.0f32; 2];
    if let Some(mouse) = doc.get("initialMousePosition") {
        let items = mouse.as_array().filter(|a| a.len() == 2).ok_or_else(|| {
            mlua::Error::RuntimeError("recording initial mouse position is invalid".into())
        })?;
        if items.iter().any(|v| {
            v.as_f64()
                .is_none_or(|n| !n.is_finite() || n.abs() > 32768.0)
        }) {
            return Err(mlua::Error::RuntimeError(
                "recording initial mouse position is invalid".into(),
            ));
        }
        initial_mouse = [
            items[0].as_f64().unwrap() as f32,
            items[1].as_f64().unwrap() as f32,
        ];
    }
    let events = doc
        .get("events")
        .and_then(Json::as_array)
        .filter(|events| events.len() <= MAX_TAPE_EVENTS)
        .ok_or_else(|| {
            mlua::Error::RuntimeError("recording events are invalid or exceed the limit".into())
        })?;
    let mut last_time = 0.0;
    let mut validated = Vec::with_capacity(events.len());
    for event in events {
        let time = event
            .get("time")
            .and_then(Json::as_f64)
            .filter(|t| t.is_finite() && *t >= last_time && *t <= duration)
            .ok_or_else(|| {
                mlua::Error::RuntimeError(
                    "recording timestamps must be ordered within duration".into(),
                )
            })?;
        last_time = time;
        let method = event
            .get("method")
            .and_then(Json::as_str)
            .ok_or_else(|| mlua::Error::RuntimeError("recording event method is missing".into()))?;
        let args = event
            .get("args")
            .and_then(Json::as_array)
            .ok_or_else(|| mlua::Error::RuntimeError("recording event args are invalid".into()))?;
        if method == "__host" {
            if args.len() != 1
                || !args[0].is_array()
                || args[0].as_array().unwrap().len() > MAX_BATCH
            {
                return Err(mlua::Error::RuntimeError(
                    "recorded host input batch is invalid".into(),
                ));
            }
            // Validate each action without dispatching it.
            for input in args[0].as_array().unwrap() {
                validate_recorded_input(input)?;
            }
        } else if method == "__gui" {
            if args.len() != 1 || !args[0].is_object() {
                return Err(mlua::Error::RuntimeError(
                    "recorded GUI action is invalid".into(),
                ));
            }
        } else if let Some(index) = VIM_METHODS.iter().position(|m| *m == method) {
            let valid_count = match index {
                0 => (3..=4).contains(&args.len()),
                1 => (4..=6).contains(&args.len()),
                2 | 3 => (2..=3).contains(&args.len()),
                4 => (3..=4).contains(&args.len()),
                5 => args.len() == 4,
                _ => (1..=2).contains(&args.len()),
            };
            if !valid_count {
                return Err(mlua::Error::RuntimeError(format!(
                    "recorded {method} has invalid argument count"
                )));
            }
            validate_vim_tape_args(method, args)?;
            // Resolve tagged values up front so bad enums and stale references
            // fail before any event from the tape is played.
            for value in args {
                let _ = decode_tape_value(lua, ctx, value)?;
            }
        } else {
            return Err(mlua::Error::RuntimeError(format!(
                "unsupported recording method: {method}"
            )));
        }
        validated.push(event.clone());
    }
    Ok((validated, duration, additional, initial_mouse))
}

fn validate_vim_tape_args(method: &str, args: &[Json]) -> mlua::Result<()> {
    let number = |index: usize| -> mlua::Result<f64> {
        args.get(index)
            .and_then(Json::as_f64)
            .filter(|n| n.is_finite() && n.abs() <= 32768.0)
            .ok_or_else(|| {
                mlua::Error::RuntimeError("recorded numeric input argument is invalid".into())
            })
    };
    let boolean = |index: usize| -> mlua::Result<bool> {
        args.get(index).and_then(Json::as_bool).ok_or_else(|| {
            mlua::Error::RuntimeError("recorded boolean input argument is invalid".into())
        })
    };
    let layer = |index: usize| -> mlua::Result<()> {
        if let Some(value) = args.get(index) {
            if !value.is_null() && value.get("$ref").and_then(Json::as_str).is_none() {
                return Err(mlua::Error::RuntimeError(
                    "recorded input layer must be an instance reference".into(),
                ));
            }
        }
        Ok(())
    };
    match method {
        "SendKeyEvent" => {
            boolean(0)?;
            boolean(2)?;
            if args
                .get(1)
                .and_then(|v| v.get("$enum"))
                .and_then(|e| e.get("type"))
                .and_then(Json::as_str)
                != Some("KeyCode")
            {
                return Err(mlua::Error::RuntimeError(
                    "recorded keyboard event requires a KeyCode".into(),
                ));
            }
            layer(3)?;
        }
        "SendMouseButtonEvent" => {
            number(0)?;
            number(1)?;
            if args[2].as_i64().is_none_or(|n| !(0..=2).contains(&n)) {
                return Err(mlua::Error::RuntimeError(
                    "recorded mouse button is invalid".into(),
                ));
            }
            boolean(3)?;
            layer(4)?;
            if args.len() > 5
                && !args[5].is_null()
                && args[5]
                    .as_i64()
                    .is_none_or(|n| !(0..=i32::MAX as i64).contains(&n))
            {
                return Err(mlua::Error::RuntimeError(
                    "recorded mouse repeat count is invalid".into(),
                ));
            }
        }
        "SendMouseMoveEvent" | "SendMouseMoveDeltaEvent" => {
            number(0)?;
            number(1)?;
            layer(2)?;
        }
        "SendMouseWheelEvent" => {
            number(0)?;
            number(1)?;
            boolean(2)?;
            layer(3)?;
        }
        "SendTouchEvent" => {
            if args[0]
                .as_i64()
                .is_none_or(|n| n.unsigned_abs() > 9_007_199_254_740_991)
                || args[1].as_i64().is_none_or(|n| !(0..=3).contains(&n))
            {
                return Err(mlua::Error::RuntimeError(
                    "recorded touch event ID or state is invalid".into(),
                ));
            }
            number(2)?;
            number(3)?;
        }
        "SendTextInputCharacterEvent" => {
            if args[0]
                .as_str()
                .is_none_or(|s| s.len() > 65536 || s.contains('\0'))
            {
                return Err(mlua::Error::RuntimeError(
                    "recorded text input is invalid".into(),
                ));
            }
            layer(1)?;
        }
        _ => {
            return Err(mlua::Error::RuntimeError(
                "unsupported recording method".into(),
            ))
        }
    }
    Ok(())
}

fn validate_recorded_input(event: &Json) -> mlua::Result<()> {
    let phase = event.get("phase").and_then(Json::as_str);
    let input_type = event.get("input_type").and_then(Json::as_str);
    let valid = matches!(phase, Some("begin" | "change" | "end" | "cancel"))
        && matches!(
            input_type,
            Some(
                "Keyboard"
                    | "MouseButton1"
                    | "MouseButton2"
                    | "MouseButton3"
                    | "MouseWheel"
                    | "MouseMovement"
                    | "Touch"
                    | "Focus"
            )
        );
    if !valid {
        return Err(mlua::Error::RuntimeError(
            "recorded host input event is invalid".into(),
        ));
    }
    Ok(())
}

fn start_playback(lua: &Lua, ctx: &WorldRef, content: &str) -> mlua::Result<()> {
    let (events, duration, additional, initial_mouse) = validate_tape(lua, ctx, content)?;
    let state = state(lua)?;
    let mut state = state.borrow_mut();
    if state.recording_started.is_some() || state.playback_started_at.is_some() {
        return Err(mlua::Error::RuntimeError(
            "stop the current recording or playback first".into(),
        ));
    }
    state.playback_events = events;
    state.playback_duration = duration;
    state.playback_cursor = 0;
    state.playback_additional_state = additional;
    state.mouse = initial_mouse;
    state.playback_last_error.clear();
    state.playback_owned.clear();
    state.playback_owned_gui_pointer = None;
    state.playback_started_at = Some(Instant::now());
    Ok(())
}

/// Advance playback against the caller's monotonic frame time. At most 128
/// due events are dispatched in one tick so a malformed/large recording cannot
/// monopolize a frame.
pub fn step(lua: &Lua, ctx: &WorldRef, now: Instant, _vm_started: Instant) -> mlua::Result<()> {
    let (due, finish) = {
        let state = state(lua)?;
        let mut state = state.borrow_mut();
        let Some(started) = state.playback_started_at else {
            return Ok(());
        };
        let elapsed = now.saturating_duration_since(started).as_secs_f64();
        let mut due = Vec::new();
        while state.playback_cursor < state.playback_events.len()
            && due.len() < MAX_PLAYBACK_PER_STEP
        {
            let event = &state.playback_events[state.playback_cursor];
            if event
                .get("time")
                .and_then(Json::as_f64)
                .unwrap_or(f64::INFINITY)
                > elapsed
            {
                break;
            }
            due.push(event.clone());
            state.playback_cursor += 1;
        }
        let finish = state.playback_cursor == state.playback_events.len()
            && elapsed >= state.playback_duration;
        (due, finish)
    };
    for event in due {
        if let Err(error) = dispatch_tape_event(lua, ctx, &event) {
            state(lua)?.borrow_mut().playback_last_error = error.to_string();
            let id = ctx
                .borrow()
                .services
                .get("VirtualInputManager")
                .cloned()
                .unwrap_or_default();
            if !id.is_empty() {
                stop_playback(lua, ctx, &id, false)?;
            }
            return Err(error);
        }
    }
    if finish {
        let id = ctx
            .borrow()
            .services
            .get("VirtualInputManager")
            .cloned()
            .unwrap_or_default();
        if !id.is_empty() {
            stop_playback(lua, ctx, &id, true)?;
        }
    }
    Ok(())
}

fn dispatch_tape_event(lua: &Lua, ctx: &WorldRef, event: &Json) -> mlua::Result<()> {
    let method = event
        .get("method")
        .and_then(Json::as_str)
        .unwrap_or_default();
    let args = event
        .get("args")
        .and_then(Json::as_array)
        .ok_or_else(|| mlua::Error::RuntimeError("invalid playback event args".into()))?;
    let manager = ctx
        .borrow()
        .services
        .get("VirtualInputManager")
        .cloned()
        .unwrap_or_default();
    let before = state(lua)?.borrow().contacts.clone();
    let gui_before = state(lua)?.borrow().gui_pointer_target.clone();
    match method {
        "__host" => action_with_origin(lua, ctx, &json!({"events":args[0]}), true)?,
        "__gui" => action_with_origin(lua, ctx, &args[0], true)?,
        method if VIM_METHODS.contains(&method) => {
            let lua_args = args
                .iter()
                .map(|v| decode_tape_value(lua, ctx, v))
                .collect::<mlua::Result<Vec<_>>>()?;
            let _ = virtual_method(lua, ctx, &manager, MultiValue::from_vec(lua_args), method)?;
        }
        _ => {
            return Err(mlua::Error::RuntimeError(
                "unsupported playback method".into(),
            ))
        }
    }
    let current = state(lua)?.borrow().contacts.clone();
    let gui_current = state(lua)?.borrow().gui_pointer_target.clone();
    let state = state(lua)?;
    let mut state = state.borrow_mut();
    for (key, contact) in current {
        if !before.contains_key(&key) {
            state.playback_owned.insert(key, contact);
        }
    }
    for key in before.keys() {
        if !state.contacts.contains_key(key) {
            state.playback_owned.remove(key);
        }
    }
    if gui_before.is_none() {
        state.playback_owned_gui_pointer = gui_current;
    } else if gui_current.is_none() {
        state.playback_owned_gui_pointer = None;
    }
    Ok(())
}

fn stop_playback(lua: &Lua, ctx: &WorldRef, id: &str, completed: bool) -> mlua::Result<()> {
    let (owned, owned_gui) = {
        let state = state(lua)?;
        let mut state = state.borrow_mut();
        if state.playback_started_at.take().is_none() {
            return Ok(());
        }
        (
            std::mem::take(&mut state.playback_owned),
            state.playback_owned_gui_pointer.take(),
        )
    };
    if let Some(gui_id) = owned_gui {
        if state(lua)?.borrow().gui_pointer_target.as_deref() == Some(gui_id.as_str()) {
            let _ = action(lua, ctx, &json!({"kind":"pointer_up","id":gui_id}));
        }
    }
    for (identity, contact) in owned {
        if !state(lua)?.borrow().contacts.contains_key(&identity) {
            continue;
        }
        let mut event = json!({"phase":"cancel","input_type":contact.input_type});
        if contact.input_type == "Keyboard" {
            event["key_code"] = json!(contact.key);
        }
        if contact.input_type == "Touch" {
            event["input_id"] = json!(identity.strip_prefix("Touch:").unwrap_or(&identity));
        }
        let _ = action(lua, ctx, &json!({"events":[event]}));
    }
    if completed {
        let additional = state(lua)?.borrow().playback_additional_state.clone();
        super::signals::fire(
            lua,
            ctx,
            id,
            "PlaybackCompleted",
            MultiValue::from_vec(vec![Value::String(lua.create_string(additional)?)]),
        )?;
    }
    let state = state(lua)?;
    let mut state = state.borrow_mut();
    state.playback_events.clear();
    state.playback_cursor = 0;
    state.playback_duration = 0.0;
    Ok(())
}
fn virtual_tape_method(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
    method: &str,
) -> mlua::Result<MultiValue> {
    if !world::is_a(ctx, id, "VirtualInputManager") {
        return Err(mlua::Error::RuntimeError(
            "live VirtualInputManager receiver required".into(),
        ));
    }
    match method {
        "StartRecording" => {
            let state = state(lua)?;
            let mut state = state.borrow_mut();
            if state.recording_started.is_some() || state.playback_started_at.is_some() {
                return Err(mlua::Error::RuntimeError(
                    "stop the current recording or playback first".into(),
                ));
            }
            state.recording_events.clear();
            state.recording_bytes = 0;
            state.recording_duration = 0.;
            state.recording_last_error.clear();
            state.recording_initial_mouse = state.mouse;
            state.recording_started = Some(Instant::now());
        }
        "StopRecording" => {
            let duration = {
                let state = state(lua)?;
                let mut state = state.borrow_mut();
                let Some(start) = state.recording_started.take() else {
                    return Ok(MultiValue::new());
                };
                let duration = start.elapsed().as_secs_f64();
                state.recording_duration = duration;
                duration
            };
            let doc = recording_document(lua)?.to_string();
            super::signals::fire(
                lua,
                ctx,
                id,
                "RecordingCompleted",
                MultiValue::from_vec(vec![Value::String(lua.create_string(doc)?)]),
            )?;
            let _ = duration;
        }
        "Dump" => {
            let doc = recording_document(lua)?.to_string();
            let print: Function = lua.globals().get("print")?;
            print.call::<()>(doc)?;
        }
        "StartPlayingJSON" | "StartPlaying" => {
            let args: Vec<Value> = args.into_iter().collect();
            if args.len() != 1 {
                return Err(mlua::Error::RuntimeError(format!(
                    "{method} expects one string argument"
                )));
            }
            let source = match &args[0] {
                Value::String(s) => s.to_str()?.to_string(),
                _ => {
                    return Err(mlua::Error::RuntimeError(
                        "recording source must be a string".into(),
                    ))
                }
            };
            let content = if method == "StartPlayingJSON" {
                source
            } else {
                let readfile: Function = lua.globals().get("readfile").map_err(|_| {
                    mlua::Error::RuntimeError("VM workspace readfile is unavailable".into())
                })?;
                readfile.call::<String>(source)?
            };
            start_playback(lua, ctx, &content)?;
        }
        "StopPlaying" => stop_playback(lua, ctx, id, false)?,
        _ => {
            return Err(mlua::Error::RuntimeError(
                "unsupported recording method".into(),
            ))
        }
    }
    Ok(MultiValue::new())
}
fn additional_state_getter(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    key: &str,
) -> mlua::Result<Option<Value>> {
    if key != "AdditionalLuaState" || !world::is_a(ctx, id, "VirtualInputManager") {
        return Ok(None);
    }
    let state = state(lua)?;
    let text = state.borrow().additional_lua_state.clone();
    Ok(Some(Value::String(lua.create_string(text)?)))
}
fn additional_state_setter(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    value: Value,
) -> mlua::Result<bool> {
    if !world::is_a(ctx, id, "VirtualInputManager") {
        return Ok(false);
    }
    let text = match value {
        Value::String(s) => s.to_str()?.to_string(),
        _ => {
            return Err(mlua::Error::RuntimeError(
                "AdditionalLuaState must be a string".into(),
            ))
        }
    };
    if text.len() > 65536 {
        return Err(mlua::Error::RuntimeError(
            "AdditionalLuaState exceeds 64 KiB".into(),
        ));
    }
    state(lua)?.borrow_mut().additional_lua_state = text;
    Ok(true)
}

const PLAYER_MODE_TYPES: [&str; 4] = [
    "ComputerCameraMovementMode",
    "ComputerMovementMode",
    "TouchCameraMovementMode",
    "TouchMovementMode",
];
const PLAYER_MODE_DEFAULTS: [[i32; 4]; 4] =
    [[0, 1, 2, 0], [0, 1, 2, 0], [0, 1, 2, 0], [0, 1, 4, 5]];
const PLAYER_MODE_COUNTS: [usize; 4] = [3, 3, 3, 4];
fn player_mode_method(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
    kind: usize,
    operation: u8,
) -> mlua::Result<MultiValue> {
    if !world::is_a(ctx, id, "PlayerScripts") {
        return Err(mlua::Error::RuntimeError(
            "live PlayerScripts receiver required".into(),
        ));
    }
    let mut output = MultiValue::new();
    if operation == 0 {
        let value = args.into_iter().next().unwrap_or(Value::Nil);
        let json = super::values::to_json(lua, &value)?;
        let enum_value = json
            .get("$enum")
            .and_then(|e| e.get("value"))
            .and_then(Json::as_i64)
            .ok_or_else(|| {
                mlua::Error::RuntimeError("matching movement-mode EnumItem required".into())
            })?;
        if json
            .get("$enum")
            .and_then(|e| e.get("type"))
            .and_then(Json::as_str)
            != Some(PLAYER_MODE_TYPES[kind])
        {
            return Err(mlua::Error::RuntimeError(
                "matching movement-mode EnumItem required".into(),
            ));
        }
        let mut state = state(lua)?;
        let mut state = state.borrow_mut();
        let modes = state
            .player_modes
            .entry(id.to_owned())
            .or_insert_with(|| PlayerModeState {
                registered: [false; 4],
                values: std::array::from_fn(|k| {
                    PLAYER_MODE_DEFAULTS[k][..PLAYER_MODE_COUNTS[k]].to_vec()
                }),
            });
        if !modes.registered[kind] {
            modes.values[kind].clear();
            modes.registered[kind] = true;
        }
        let value = enum_value as i32;
        if !modes.values[kind].contains(&value) {
            modes.values[kind].push(value);
            modes.values[kind].sort_unstable();
        }
        drop(state);
        super::signals::fire(
            lua,
            ctx,
            id,
            &format!("{}Registered", PLAYER_MODE_TYPES[kind]),
            MultiValue::new(),
        )?;
    } else if operation == 1 {
        let mut state = state(lua)?;
        let mut state = state.borrow_mut();
        let modes = state
            .player_modes
            .entry(id.to_owned())
            .or_insert_with(|| PlayerModeState {
                registered: [false; 4],
                values: std::array::from_fn(|k| {
                    PLAYER_MODE_DEFAULTS[k][..PLAYER_MODE_COUNTS[k]].to_vec()
                }),
            });
        modes.values[kind].clear();
    } else {
        let values = {
            let mut state = state(lua)?;
            let mut state = state.borrow_mut();
            let modes =
                state
                    .player_modes
                    .entry(id.to_owned())
                    .or_insert_with(|| PlayerModeState {
                        registered: [false; 4],
                        values: std::array::from_fn(|k| {
                            PLAYER_MODE_DEFAULTS[k][..PLAYER_MODE_COUNTS[k]].to_vec()
                        }),
                    });
            modes.values[kind].clone()
        };
        let names: &[&str] = match kind {
            0 => &["Default", "Classic", "Follow", "Orbital", "CameraToggle"],
            1 => &["Default", "KeyboardMouse", "ClickToMove"],
            2 => &["Default", "Classic", "Follow", "Orbital"],
            _ => &[
                "Default",
                "Thumbstick",
                "DPad",
                "Thumbpad",
                "ClickToMove",
                "DynamicThumbstick",
            ],
        };
        let table = lua.create_table()?;
        for (index, value) in values.iter().enumerate() {
            let name = names
                .iter()
                .find(|name| {
                    super::enums::enum_item(lua, PLAYER_MODE_TYPES[kind], name)
                        .ok()
                        .and_then(|item| super::enums::enum_item_value(&item).ok())
                        == Some(*value)
                })
                .ok_or_else(|| mlua::Error::RuntimeError("invalid stored movement mode".into()))?;
            table.set(
                index + 1,
                super::enums::enum_item(lua, PLAYER_MODE_TYPES[kind], name)?,
            )?;
        }
        output.push_back(Value::Table(table));
    }
    Ok(output)
}
macro_rules! player_mode_method {
    ($fn_name:ident,$kind:literal,$op:literal) => {
        fn $fn_name(
            lua: &Lua,
            ctx: &WorldRef,
            id: &str,
            args: MultiValue,
        ) -> mlua::Result<MultiValue> {
            player_mode_method(lua, ctx, id, args, $kind, $op)
        }
    };
}
player_mode_method!(register_computer_camera_mode, 0, 0);
player_mode_method!(clear_computer_camera_modes, 0, 1);
player_mode_method!(get_computer_camera_modes, 0, 2);
player_mode_method!(register_computer_mode, 1, 0);
player_mode_method!(clear_computer_modes, 1, 1);
player_mode_method!(get_computer_modes, 1, 2);
player_mode_method!(register_touch_camera_mode, 2, 0);
player_mode_method!(clear_touch_camera_modes, 2, 1);
player_mode_method!(get_touch_camera_modes, 2, 2);
player_mode_method!(register_touch_mode, 3, 0);
player_mode_method!(clear_touch_modes, 3, 1);
player_mode_method!(get_touch_modes, 3, 2);

fn virtual_method(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    args: MultiValue,
    operation: &str,
) -> mlua::Result<MultiValue> {
    if !world::is_a(ctx, id, "VirtualInputManager") {
        return Err(mlua::Error::RuntimeError(
            "live VirtualInputManager receiver required".into(),
        ));
    }
    let args: Vec<Value> = args.into_iter().collect();
    let mut event = serde_json::Map::new();
    let mut key_for_text: Option<String> = None;
    let mut key_down = false;
    match operation {
        "SendKeyEvent" => {
            if args.len() < 3 || args.len() > 4 {
                return Err(mlua::Error::RuntimeError(
                    "SendKeyEvent expects pressed, KeyCode, repeated and optional layer".into(),
                ));
            }
            let down = match args[0] {
                Value::Boolean(v) => v,
                _ => return Err(mlua::Error::RuntimeError("pressed must be boolean".into())),
            };
            let key = super::values::to_json(lua, &args[1])?;
            let kind = key
                .get("$enum")
                .and_then(|v| v.get("type"))
                .and_then(Json::as_str);
            let name = key
                .get("$enum")
                .and_then(|v| v.get("name"))
                .and_then(Json::as_str);
            if kind != Some("KeyCode") || name.is_none_or(|v| v == "None") {
                return Err(mlua::Error::RuntimeError("keyboard key required".into()));
            }
            let repeated = bool_arg(&args[2], "repeated")?;
            key_for_text = Some(name.unwrap().to_owned());
            key_down = down;
            let identity = format!("key:{}", name.unwrap());
            let held = {
                let s = state(lua)?;
                let s = s.borrow();
                s.contacts.contains_key(&identity)
            };
            if down && held && !repeated {
                return Ok(MultiValue::new());
            }
            if !down && !held {
                return Ok(MultiValue::new());
            }
            event.insert(
                "phase".into(),
                json!(if down {
                    if held {
                        "change"
                    } else {
                        "begin"
                    }
                } else {
                    "end"
                }),
            );
            event.insert("input_type".into(), json!("Keyboard"));
            event.insert("key_code".into(), json!(name.unwrap()));
        }
        "SendMouseButtonEvent" => {
            if args.len() < 4 || args.len() > 6 {
                return Err(mlua::Error::RuntimeError("SendMouseButtonEvent expects x, y, button, pressed and optional layer/repeat count".into()));
            }
            let x = number_arg(&args[0], "mouse x")?;
            let y = number_arg(&args[1], "mouse y")?;
            let button = int_arg(&args[2], "mouse button")?;
            if !(0..=2).contains(&button) {
                return Err(mlua::Error::RuntimeError(
                    "mouse button must be 0, 1, or 2".into(),
                ));
            }
            let down = bool_arg(&args[3], "pressed")?;
            let input_type = format!("MouseButton{}", button + 1);
            let held = {
                let s = state(lua)?;
                let s = s.borrow();
                s.contacts.contains_key(&input_type)
            };
            if down == held {
                return Ok(MultiValue::new());
            }
            event.insert("phase".into(), json!(if down { "begin" } else { "end" }));
            event.insert("input_type".into(), json!(input_type));
            event.insert("position".into(), json!([x, y, 0]));
            event.insert("delta".into(), json!([0, 0, 0]));
            event.insert("mouse_location".into(), json!([x, y]));
        }
        "SendMouseMoveEvent" | "SendMouseMoveDeltaEvent" => {
            if args.len() < 2 || args.len() > 3 {
                return Err(mlua::Error::RuntimeError(
                    "mouse move expects x, y and optional layer".into(),
                ));
            }
            let x = number_arg(&args[0], "mouse x")?;
            let y = number_arg(&args[1], "mouse y")?;
            let previous = {
                let s = state(lua)?;
                let s = s.borrow();
                s.mouse
            };
            let (px, py, dx, dy) = if operation == "SendMouseMoveDeltaEvent" {
                (previous[0] as f64 + x, previous[1] as f64 + y, x, y)
            } else {
                (x, y, x - previous[0] as f64, y - previous[1] as f64)
            };
            event.insert("phase".into(), json!("change"));
            event.insert("input_type".into(), json!("MouseMovement"));
            event.insert("position".into(), json!([px, py, 0]));
            event.insert("delta".into(), json!([dx, dy, 0]));
            event.insert("mouse_location".into(), json!([px, py]));
        }
        "SendMouseWheelEvent" => {
            if args.len() < 3 || args.len() > 4 {
                return Err(mlua::Error::RuntimeError(
                    "mouse wheel expects x, y, forward and optional layer".into(),
                ));
            }
            let x = number_arg(&args[0], "mouse x")?;
            let y = number_arg(&args[1], "mouse y")?;
            let forward = bool_arg(&args[2], "forward")?;
            let step = if forward { 1 } else { -1 };
            event.insert("phase".into(), json!("change"));
            event.insert("input_type".into(), json!("MouseWheel"));
            event.insert("position".into(), json!([x, y, step]));
            event.insert("delta".into(), json!([0, 0, step]));
            event.insert("mouse_location".into(), json!([x, y]));
        }
        "SendTouchEvent" => {
            if args.len() != 4 {
                return Err(mlua::Error::RuntimeError(
                    "SendTouchEvent expects id, state, x and y".into(),
                ));
            }
            let touch = int_arg(&args[0], "touch id")?;
            let touch_state = int_arg(&args[1], "touch state")?;
            if !(0..=3).contains(&touch_state) {
                return Err(mlua::Error::RuntimeError(
                    "touch state must be Begin, Change, End, or Cancel".into(),
                ));
            }
            let x = number_arg(&args[2], "touch x")?;
            let y = number_arg(&args[3], "touch y")?;
            let token = format!("virtual:{touch}");
            event.insert(
                "phase".into(),
                json!(match touch_state {
                    0 => "begin",
                    1 => "change",
                    2 => "end",
                    _ => "cancel",
                }),
            );
            event.insert("input_type".into(), json!("Touch"));
            event.insert("input_id".into(), json!(token));
            event.insert("position".into(), json!([x, y, 0]));
        }
        "SendTextInputCharacterEvent" => {
            if args.is_empty() || args.len() > 2 {
                return Err(mlua::Error::RuntimeError(
                    "SendTextInputCharacterEvent expects text and optional layer".into(),
                ));
            }
            let text = match &args[0] {
                Value::String(s) => s.to_str()?.to_string(),
                _ => {
                    return Err(mlua::Error::RuntimeError(
                        "text input must be a string".into(),
                    ))
                }
            };
            if text.len() > 65536 || text.contains('\0') {
                return Err(mlua::Error::RuntimeError(
                    "text input must be valid UTF-8 within 64 KiB".into(),
                ));
            }
            let focused = ctx.borrow().focused_textbox.clone();
            let Some(box_id) = focused else {
                return Ok(MultiValue::new());
            };
            if world::get_property(ctx, &box_id, "TextEditable")
                .ok()
                .and_then(|v| v.as_bool())
                == Some(false)
                || text.is_empty()
            {
                return Ok(MultiValue::new());
            }
            let old = world::get_property(ctx, &box_id, "Text")
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default();
            let (cursor, anchor) = {
                let w = ctx.borrow();
                (w.focused_cursor, w.focused_anchor)
            };
            let mut start = byte_boundary(
                &old,
                if cursor > 0 {
                    cursor as usize
                } else {
                    old.len() + 1
                },
            )
            .saturating_sub(1);
            let mut end = start;
            if anchor > 0 {
                let other = byte_boundary(&old, anchor as usize).saturating_sub(1);
                start = start.min(other);
                end = end.max(other);
            }
            if old
                .len()
                .saturating_sub(end - start)
                .saturating_add(text.len())
                > 65536
            {
                return Err(mlua::Error::RuntimeError(
                    "text input exceeds the local editor limit".into(),
                ));
            }
            let mut next = String::with_capacity(old.len() + text.len());
            next.push_str(&old[..start]);
            next.push_str(&text);
            next.push_str(&old[end..]);
            world::set_property(lua, ctx, &box_id, "Text", json!(next))?;
            let cursor = start
                .saturating_add(text.len())
                .saturating_add(1)
                .min(i32::MAX as usize) as i32;
            let mut w = ctx.borrow_mut();
            w.focused_text = next;
            w.focused_cursor = cursor;
            w.focused_anchor = -1;
            w.focused_editor_revision = w.focused_editor_revision.wrapping_add(1);
        }
        _ => {
            return Err(mlua::Error::RuntimeError(
                "unsupported VirtualInputManager method".into(),
            ))
        }
    }
    let old_caps;
    let old_preferred;
    {
        let s = state(lua)?;
        let mut s = s.borrow_mut();
        old_caps = s.capabilities.clone();
        old_preferred = s.preferred.clone();
        for key in ["KeyboardEnabled", "MouseEnabled", "TouchEnabled"] {
            s.capabilities.insert(key.into(), true);
        }
    }
    let payload = json!({"events":[Json::Object(event)]});
    let result = action_with_origin(lua, ctx, &payload, true);
    {
        let s = state(lua)?;
        let mut s = s.borrow_mut();
        s.capabilities = old_caps;
        s.preferred = old_preferred;
    }
    result?;
    if key_down {
        if let Some(key) = key_for_text {
            edit_focused_textbox_key(lua, ctx, &key)?;
        }
    }
    Ok(MultiValue::new())
}

fn edit_focused_textbox_key(lua: &Lua, ctx: &WorldRef, key: &str) -> mlua::Result<()> {
    let Some(id) = focused_textbox(ctx) else {
        return Ok(());
    };
    let (mut text, cursor, selection) = {
        let w = ctx.borrow();
        (w.focused_text.clone(), w.focused_cursor, w.focused_anchor)
    };
    let input = state(lua)?;
    let input = input.borrow();
    let control = ["LeftControl", "RightControl", "LeftMeta", "RightMeta"]
        .iter()
        .any(|key| input.contacts.contains_key(&format!("key:{key}")));
    let shift = ["LeftShift", "RightShift"]
        .iter()
        .any(|key| input.contacts.contains_key(&format!("key:{key}")));
    drop(input);
    let cursor = byte_boundary(
        &text,
        if cursor > 0 {
            cursor as usize
        } else {
            text.len() + 1
        },
    ) as i32;
    let anchor = if selection > 0 {
        byte_boundary(&text, selection as usize) as i32
    } else {
        cursor
    };
    if key == "Escape" {
        return release_textbox(lua, ctx, &id, false);
    }
    if key == "Return" || key == "KeypadEnter" {
        if world::get_property(ctx, &id, "MultiLine")
            .ok()
            .and_then(|v| v.as_bool())
            == Some(true)
        {
            return insert_text_at_selection(lua, ctx, &id, &text, cursor, anchor, "\n");
        }
        return release_textbox(lua, ctx, &id, true);
    }
    if key == "A" && control {
        let mut w = ctx.borrow_mut();
        w.focused_cursor = text.len() as i32 + 1;
        w.focused_anchor = if text.is_empty() { -1 } else { 1 };
        return Ok(());
    }
    if key == "Backspace" || key == "Delete" {
        if world::get_property(ctx, &id, "TextEditable")
            .ok()
            .and_then(|v| v.as_bool())
            == Some(false)
        {
            return Ok(());
        }
        let mut begin = cursor.min(anchor);
        let mut end = cursor.max(anchor);
        if begin == end {
            if key == "Backspace" {
                begin = previous_boundary(&text, begin) as i32;
            } else {
                end = next_boundary(&text, end) as i32;
            }
        }
        if begin != end {
            let start = (begin.max(1) - 1) as usize;
            let stop = (end.max(1) - 1) as usize;
            text.replace_range(start..stop, "");
            world::set_property(lua, ctx, &id, "Text", json!(text.clone()))?;
            let mut w = ctx.borrow_mut();
            w.focused_text = text;
            w.focused_cursor = begin;
            w.focused_anchor = -1;
            w.focused_editor_revision = w.focused_editor_revision.wrapping_add(1);
        }
        return Ok(());
    }
    let target = match key {
        "Left" => {
            if !shift && anchor != cursor {
                anchor.min(cursor)
            } else {
                previous_boundary(&text, cursor) as i32
            }
        }
        "Right" => {
            if !shift && anchor != cursor {
                anchor.max(cursor)
            } else {
                next_boundary(&text, cursor) as i32
            }
        }
        "Home" => {
            if control {
                1
            } else {
                text[..(cursor - 1).max(0) as usize]
                    .rfind('\n')
                    .map(|i| i as i32 + 2)
                    .unwrap_or(1)
            }
        }
        "End" => {
            if control {
                text.len() as i32 + 1
            } else {
                text[cursor.max(1) as usize - 1..]
                    .find('\n')
                    .map(|i| cursor + i as i32)
                    .unwrap_or(text.len() as i32 + 1)
            }
        }
        _ => return Ok(()),
    };
    let mut w = ctx.borrow_mut();
    w.focused_cursor = target;
    w.focused_anchor = if shift {
        if selection > 0 {
            selection
        } else {
            cursor
        }
    } else {
        -1
    };
    Ok(())
}

fn previous_boundary(text: &str, position: i32) -> usize {
    let mut offset = (position.max(1) - 1) as usize;
    offset = offset.min(text.len());
    if offset > 0 {
        offset -= 1;
        while offset > 0 && (text.as_bytes()[offset] & 0xc0) == 0x80 {
            offset -= 1;
        }
    }
    offset + 1
}
fn next_boundary(text: &str, position: i32) -> usize {
    let mut offset = (position.max(1) - 1) as usize;
    offset = offset.min(text.len());
    if offset < text.len() {
        offset += 1;
        while offset < text.len() && (text.as_bytes()[offset] & 0xc0) == 0x80 {
            offset += 1;
        }
    }
    offset + 1
}
fn insert_text_at_selection(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    old: &str,
    cursor: i32,
    anchor: i32,
    value: &str,
) -> mlua::Result<()> {
    if world::get_property(ctx, id, "TextEditable")
        .ok()
        .and_then(|v| v.as_bool())
        == Some(false)
    {
        return Ok(());
    }
    let start = (cursor.min(anchor).max(1) - 1) as usize;
    let end = (cursor.max(anchor).max(1) - 1) as usize;
    let mut text = old.to_owned();
    text.replace_range(start..end, value);
    world::set_property(lua, ctx, id, "Text", json!(text.clone()))?;
    let mut w = ctx.borrow_mut();
    w.focused_text = text;
    w.focused_cursor = (start + value.len() + 1).min(i32::MAX as usize) as i32;
    w.focused_anchor = -1;
    w.focused_editor_revision = w.focused_editor_revision.wrapping_add(1);
    Ok(())
}

fn number_arg(value: &Value, name: &str) -> mlua::Result<f64> {
    let n = match value {
        Value::Number(v) => *v,
        Value::Integer(v) => *v as f64,
        _ => {
            return Err(mlua::Error::RuntimeError(format!(
                "{name} must be a number"
            )))
        }
    };
    if !n.is_finite() || n.abs() > 32768. {
        return Err(mlua::Error::RuntimeError(format!(
            "{name} must be finite and bounded"
        )));
    }
    Ok(n)
}
fn int_arg(value: &Value, name: &str) -> mlua::Result<i64> {
    let n = number_arg(value, name)?;
    if n.fract() != 0. {
        return Err(mlua::Error::RuntimeError(format!(
            "{name} must be an integer"
        )));
    }
    Ok(n as i64)
}
fn bool_arg(value: &Value, name: &str) -> mlua::Result<bool> {
    match value {
        Value::Boolean(v) => Ok(*v),
        _ => Err(mlua::Error::RuntimeError(format!("{name} must be boolean"))),
    }
}
fn byte_boundary(text: &str, position: usize) -> usize {
    let mut at = position.saturating_sub(1).min(text.len());
    while at > 0 && at < text.len() && (text.as_bytes()[at] & 0xc0) == 0x80 {
        at -= 1;
    }
    at + 1
}
fn vim_key(lua: &Lua, c: &WorldRef, i: &str, a: MultiValue) -> mlua::Result<MultiValue> {
    virtual_method(lua, c, i, a, "SendKeyEvent")
}
fn vim_button(lua: &Lua, c: &WorldRef, i: &str, a: MultiValue) -> mlua::Result<MultiValue> {
    virtual_method(lua, c, i, a, "SendMouseButtonEvent")
}
fn vim_move(lua: &Lua, c: &WorldRef, i: &str, a: MultiValue) -> mlua::Result<MultiValue> {
    virtual_method(lua, c, i, a, "SendMouseMoveEvent")
}
fn vim_move_delta(lua: &Lua, c: &WorldRef, i: &str, a: MultiValue) -> mlua::Result<MultiValue> {
    virtual_method(lua, c, i, a, "SendMouseMoveDeltaEvent")
}
fn vim_wheel(lua: &Lua, c: &WorldRef, i: &str, a: MultiValue) -> mlua::Result<MultiValue> {
    virtual_method(lua, c, i, a, "SendMouseWheelEvent")
}
fn vim_touch(lua: &Lua, c: &WorldRef, i: &str, a: MultiValue) -> mlua::Result<MultiValue> {
    virtual_method(lua, c, i, a, "SendTouchEvent")
}
fn vim_text(lua: &Lua, c: &WorldRef, i: &str, a: MultiValue) -> mlua::Result<MultiValue> {
    virtual_method(lua, c, i, a, "SendTextInputCharacterEvent")
}
fn vim_record_start(lua: &Lua, c: &WorldRef, i: &str, a: MultiValue) -> mlua::Result<MultiValue> {
    virtual_tape_method(lua, c, i, a, "StartRecording")
}
fn vim_record_stop(lua: &Lua, c: &WorldRef, i: &str, a: MultiValue) -> mlua::Result<MultiValue> {
    virtual_tape_method(lua, c, i, a, "StopRecording")
}
fn vim_record_dump(lua: &Lua, c: &WorldRef, i: &str, a: MultiValue) -> mlua::Result<MultiValue> {
    virtual_tape_method(lua, c, i, a, "Dump")
}
fn vim_record_play_json(
    lua: &Lua,
    c: &WorldRef,
    i: &str,
    a: MultiValue,
) -> mlua::Result<MultiValue> {
    virtual_tape_method(lua, c, i, a, "StartPlayingJSON")
}
fn vim_record_play(lua: &Lua, c: &WorldRef, i: &str, a: MultiValue) -> mlua::Result<MultiValue> {
    virtual_tape_method(lua, c, i, a, "StartPlaying")
}
fn vim_record_stop_playing(
    lua: &Lua,
    c: &WorldRef,
    i: &str,
    a: MultiValue,
) -> mlua::Result<MultiValue> {
    virtual_tape_method(lua, c, i, a, "StopPlaying")
}

fn dispatch_context_actions(
    lua: &Lua,
    ctx: &WorldRef,
    service: &str,
    input_id: &str,
    input_type: &str,
    key: &str,
    state_name: &str,
    processed: bool,
) -> mlua::Result<()> {
    if processed && state_name != "End" && state_name != "Cancel" {
        return Ok(());
    }
    let mut bindings: Vec<(String, ContextBinding)> = {
        let state = state(lua)?;
        let state = state.borrow();
        state
            .context_actions
            .iter()
            .map(|(n, b)| (n.clone(), b.clone()))
            .collect()
    };
    bindings.retain(|(_, binding)| {
        binding
            .inputs
            .iter()
            .any(|(kind, value)| match kind.as_str() {
                "KeyCode" => input_type == "Keyboard" && value == key,
                "UserInputType" => value == input_type,
                "string" => {
                    input_type == "Keyboard" && value.len() == 1 && value.eq_ignore_ascii_case(key)
                }
                "PlayerActions" => {
                    input_type == "Keyboard"
                        && match value.as_str() {
                            "CharacterForward" => matches!(key, "W" | "Up"),
                            "CharacterBackward" => matches!(key, "S" | "Down"),
                            "CharacterLeft" => matches!(key, "A" | "Left"),
                            "CharacterRight" => matches!(key, "D" | "Right"),
                            "CharacterJump" => key == "Space",
                            _ => false,
                        }
                }
                _ => false,
            })
    });
    bindings.sort_by(|a, b| {
        b.1.priority
            .cmp(&a.1.priority)
            .then_with(|| b.1.sequence.cmp(&a.1.sequence))
    });
    if bindings.is_empty() {
        return Ok(());
    }
    let dispatcher = lua
        .app_data_ref::<ContextDispatcher>()
        .map(|d| d.0.clone())
        .ok_or_else(|| {
            mlua::Error::RuntimeError("context action dispatcher is not installed".into())
        })?;
    let list = lua.create_table_with_capacity(bindings.len(), 0)?;
    for (index, (name, binding)) in bindings.iter().enumerate() {
        let row = lua.create_table()?;
        row.set(1, name.as_str())?;
        row.set(2, binding.callback.clone())?;
        list.set(index + 1, row)?;
    }
    let input_state = state_name;
    let enum_value = super::enums::enum_item(lua, "UserInputState", input_state)?;
    super::scheduler::enqueue(
        lua,
        dispatcher,
        MultiValue::from_vec(vec![
            Value::Table(list),
            Value::UserData(enum_value),
            world::instance(lua, ctx, input_id)?,
        ]),
    )?;
    let _ = service;
    Ok(())
}

pub fn install(lua: &Lua, ctx: &WorldRef) -> mlua::Result<()> {
    let mut initial = InputState::default();
    if let Some(caps) = ctx
        .borrow()
        .snapshot
        .get("input_capabilities")
        .and_then(Json::as_object)
    {
        for (key, value) in caps {
            if let Some(enabled) = value.as_bool() {
                initial.capabilities.insert(key.clone(), enabled);
            }
        }
    }
    lua.set_app_data(InputStateRef(Rc::new(RefCell::new(initial))));
    let dispatcher: Function = lua
        .load(
            r#"return function(bindings, inputState, inputObject)
        for _, binding in ipairs(bindings) do
            local result = binding[2](binding[1], inputState, inputObject)
            if result == Enum.ContextActionResult.Sink then return end
        end
    end"#,
        )
        .eval()?;
    lua.set_app_data(ContextDispatcher(dispatcher));
    for (key, func) in [
        ("GetKeysPressed", method_keys as world::Method),
        ("GetMouseButtonsPressed", method_buttons),
        ("IsKeyDown", method_keydown),
        ("IsMouseButtonPressed", method_buttondown),
        ("GetLastInputType", method_last),
        ("GetFocusedTextBox", method_focused),
        ("GetMouseLocation", method_location),
        ("GetMouseDelta", method_delta),
    ] {
        world::register_method(ctx, "UserInputService", key, func);
    }
    for (name, func) in [
        ("BindAction", cas_bind as world::Method),
        ("BindActionAtPriority", cas_bind_priority),
        ("BindActionToInputTypes", cas_bind_types),
        ("UnbindAction", cas_unbind),
        ("UnbindAllActions", cas_unbind_all),
        ("GetBoundActionInfo", cas_info),
        ("GetAllBoundActionInfo", cas_all_info),
        ("GetButton", cas_button),
        ("SetDescription", cas_description),
        ("SetTitle", cas_title),
        ("SetImage", cas_image),
        ("SetPosition", cas_position),
    ] {
        world::register_method(ctx, "ContextActionService", name, func);
    }
    for (name, func) in [
        (
            "RegisterComputerCameraMovementMode",
            register_computer_camera_mode as world::Method,
        ),
        (
            "ClearComputerCameraMovementModes",
            clear_computer_camera_modes,
        ),
        (
            "GetRegisteredComputerCameraMovementModes",
            get_computer_camera_modes,
        ),
        ("RegisterComputerMovementMode", register_computer_mode),
        ("ClearComputerMovementModes", clear_computer_modes),
        ("GetRegisteredComputerMovementModes", get_computer_modes),
        (
            "RegisterTouchCameraMovementMode",
            register_touch_camera_mode,
        ),
        ("ClearTouchCameraMovementModes", clear_touch_camera_modes),
        (
            "GetRegisteredTouchCameraMovementModes",
            get_touch_camera_modes,
        ),
        ("RegisterTouchMovementMode", register_touch_mode),
        ("ClearTouchMovementModes", clear_touch_modes),
        ("GetRegisteredTouchMovementModes", get_touch_modes),
    ] {
        world::register_method(ctx, "PlayerScripts", name, func);
    }
    for (name, func) in [
        ("SendKeyEvent", vim_key as world::Method),
        ("SendMouseButtonEvent", vim_button),
        ("SendMouseMoveEvent", vim_move),
        ("SendMouseMoveDeltaEvent", vim_move_delta),
        ("SendMouseWheelEvent", vim_wheel),
        ("SendTouchEvent", vim_touch),
        ("SendTextInputCharacterEvent", vim_text),
    ] {
        world::register_method(ctx, "VirtualInputManager", name, func);
    }
    for (name, func) in [
        ("StartRecording", vim_record_start as world::Method),
        ("StopRecording", vim_record_stop),
        ("StartPlayingJSON", vim_record_play_json),
        ("StartPlaying", vim_record_play),
        ("StopPlaying", vim_record_stop_playing),
        ("Dump", vim_record_dump),
    ] {
        world::register_method(ctx, "VirtualInputManager", name, func);
    }
    world::register_keyed_getter(ctx, "AdditionalLuaState", additional_state_getter);
    world::register_setter(ctx, "AdditionalLuaState", additional_state_setter);
    for key in [
        "PreferredInput",
        "LastInputType",
        "TouchEnabled",
        "KeyboardEnabled",
        "MouseEnabled",
        "GamepadEnabled",
        "AccelerometerEnabled",
        "GyroscopeEnabled",
        "VREnabled",
        "MouseBehavior",
        "MouseIconEnabled",
        "MouseDeltaSensitivity",
    ] {
        world::register_keyed_getter(ctx, key, getter);
    }
    world::register_setter(ctx, "MouseBehavior", set_mouse_behavior);
    world::register_setter(ctx, "MouseIconEnabled", set_mouse_icon_enabled);
    world::register_setter(ctx, "MouseDeltaSensitivity", set_mouse_sensitivity);
    world::register_keyed_getter(ctx, "KeyCode", input_getter);
    world::register_setter(ctx, "KeyCode", input_set_key);
    world::register_keyed_getter(ctx, "UserInputType", input_getter);
    world::register_setter(ctx, "UserInputType", input_set_type);
    world::register_keyed_getter(ctx, "UserInputState", input_getter);
    world::register_setter(ctx, "UserInputState", input_set_state);
    world::register_keyed_getter(ctx, "Position", input_getter);
    world::register_setter(ctx, "Position", input_set_position);
    world::register_keyed_getter(ctx, "Delta", input_getter);
    world::register_setter(ctx, "Delta", input_set_delta);
    Ok(())
}

fn input_getter(lua: &Lua, ctx: &WorldRef, id: &str, key: &str) -> mlua::Result<Option<Value>> {
    get_input(lua, ctx, id, key)
}
fn set_mouse_behavior(lua: &Lua, ctx: &WorldRef, id: &str, value: Value) -> mlua::Result<bool> {
    setter(lua, ctx, id, value, "MouseBehavior")
}
fn set_mouse_icon_enabled(lua: &Lua, ctx: &WorldRef, id: &str, value: Value) -> mlua::Result<bool> {
    setter(lua, ctx, id, value, "MouseIconEnabled")
}
fn set_mouse_sensitivity(lua: &Lua, ctx: &WorldRef, id: &str, value: Value) -> mlua::Result<bool> {
    setter(lua, ctx, id, value, "MouseDeltaSensitivity")
}
fn input_set_key(lua: &Lua, ctx: &WorldRef, id: &str, value: Value) -> mlua::Result<bool> {
    set_input(lua, ctx, id, value, "KeyCode")
}
fn input_set_type(lua: &Lua, ctx: &WorldRef, id: &str, value: Value) -> mlua::Result<bool> {
    set_input(lua, ctx, id, value, "UserInputType")
}
fn input_set_state(lua: &Lua, ctx: &WorldRef, id: &str, value: Value) -> mlua::Result<bool> {
    set_input(lua, ctx, id, value, "UserInputState")
}
fn input_set_position(lua: &Lua, ctx: &WorldRef, id: &str, value: Value) -> mlua::Result<bool> {
    set_input(lua, ctx, id, value, "Position")
}
fn input_set_delta(lua: &Lua, ctx: &WorldRef, id: &str, value: Value) -> mlua::Result<bool> {
    set_input(lua, ctx, id, value, "Delta")
}

/// Update device availability from a trusted local host capability snapshot.
pub fn update_capabilities(lua: &Lua, patch: &Json) -> mlua::Result<()> {
    let object = patch.as_object().filter(|o| o.len() <= 7).ok_or_else(|| {
        mlua::Error::RuntimeError("input capabilities must be a bounded object".into())
    })?;
    let allowed = [
        "TouchEnabled",
        "KeyboardEnabled",
        "MouseEnabled",
        "GamepadEnabled",
        "AccelerometerEnabled",
        "GyroscopeEnabled",
        "VREnabled",
    ];
    let current = state(lua)?;
    let mut current = current.borrow_mut();
    for (key, value) in object {
        if !allowed.contains(&key.as_str()) || !value.is_boolean() {
            return Err(mlua::Error::RuntimeError(
                "invalid local input capability".into(),
            ));
        }
        current
            .capabilities
            .insert(key.clone(), value.as_bool().unwrap());
    }
    Ok(())
}
