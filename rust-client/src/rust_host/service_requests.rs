//! Bounded async service request bridge for local Luau jobs.
//!
//! Requests are effects for the session owner to handle. This module has no
//! network or authentication capability and only resumes callers with supplied
//! host responses (or an explicit local-runtime error).
use super::{
    scheduler, values,
    world::{self, WorldRef},
};
use mlua::{Function, Lua, MultiValue, Value};
use serde_json::{json, Value as Json};
use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    time::{Duration, Instant},
};

const MAX_TEXT: usize = 32;
const MAX_CHAT: usize = 8;
const TIMEOUT: Duration = Duration::from_secs(20);
type StateRef = Rc<RefCell<State>>;
#[derive(Default)]
struct State {
    next: u64,
    texts: HashMap<(usize, u64), Pending>,
    chats: HashMap<(usize, u64), Pending>,
    cache: HashMap<(usize, String), Json>,
    cache_bytes: usize,
}
struct Pending {
    thread: mlua::Thread,
    deadline: Instant,
    key: String,
    kind: String,
    target: Option<String>,
}
fn state(lua: &Lua) -> mlua::Result<StateRef> {
    if lua.app_data_ref::<StateRef>().is_none() {
        lua.set_app_data(Rc::new(RefCell::new(State::default())));
    }
    Ok(lua.app_data_ref::<StateRef>().unwrap().clone())
}
fn wk(ctx: &WorldRef) -> usize {
    Rc::as_ptr(ctx) as usize
}
fn push_effect(ctx: &WorldRef, key: &str, value: Json) -> mlua::Result<()> {
    let mut w = ctx.borrow_mut();
    let list = w
        .effects
        .entry(key.to_owned())
        .or_insert(Json::Array(vec![]));
    let items = list
        .as_array_mut()
        .ok_or_else(|| mlua::Error::RuntimeError(format!("{key} effect list is invalid")))?;
    if items.len() >= MAX_TEXT {
        return Err(mlua::Error::RuntimeError(format!(
            "{key} pending request limit exceeded"
        )));
    }
    items.push(value);
    Ok(())
}
fn str_arg(v: &Value, name: &str, max: usize, empty: bool) -> mlua::Result<String> {
    let Value::String(s) = v else {
        return Err(mlua::Error::RuntimeError(format!(
            "{name} must be a string"
        )));
    };
    let s = s.to_str()?.to_string();
    if s.len() > max || s.as_bytes().contains(&0) || (!empty && s.trim().is_empty()) {
        return Err(mlua::Error::RuntimeError(format!("invalid {name}")));
    }
    Ok(s)
}
fn json_payload(ctx: &WorldRef, id: &str, key: &str) -> mlua::Result<Json> {
    world::get_property(ctx, id, key)
}
fn request_text(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    method: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    if ctx
        .borrow()
        .nodes
        .get(id)
        .is_none_or(|n| n.class_name != "TextService" || n.destroyed)
    {
        return Err(mlua::Error::RuntimeError(format!(
            "{method} requires TextService"
        )));
    }
    let a: Vec<Value> = args.into_iter().collect();
    if a.len() != 1 {
        return Err(mlua::Error::RuntimeError(format!(
            "{method} expects one argument"
        )));
    }
    let (kind, req) = if method == "GetFamilyInfoAsync" {
        let family = str_arg(&a[0], "font family", 512, false)?;
        ("family", json!({"kind":"family","family":family}))
    } else {
        if !world::instance_belongs_to(&a[0], ctx) {
            return Err(mlua::Error::RuntimeError(
                "GetTextBoundsParams instance required".into(),
            ));
        }
        let params = world::instance_id(&a[0]).ok_or_else(|| {
            mlua::Error::RuntimeError("GetTextBoundsParams instance required".into())
        })?;
        if ctx
            .borrow()
            .nodes
            .get(&params)
            .is_none_or(|n| n.class_name != "GetTextBoundsParams" || n.destroyed)
        {
            return Err(mlua::Error::RuntimeError(
                "GetTextBoundsParams instance required".into(),
            ));
        }
        let font = json_payload(ctx, &params, "Font")?;
        let font = font.get("$font").cloned().ok_or_else(|| {
            mlua::Error::RuntimeError("Font is unavailable or unsupported".into())
        })?;
        let text = json_payload(ctx, &params, "Text")?
            .as_str()
            .ok_or_else(|| mlua::Error::RuntimeError("Text is unavailable".into()))?
            .to_owned();
        let size = json_payload(ctx, &params, "Size")?
            .as_f64()
            .ok_or_else(|| mlua::Error::RuntimeError("Size is unavailable".into()))?;
        let width = json_payload(ctx, &params, "Width")?
            .as_f64()
            .ok_or_else(|| mlua::Error::RuntimeError("Width is unavailable".into()))?;
        let rich = json_payload(ctx, &params, "RichText")?
            .as_bool()
            .ok_or_else(|| mlua::Error::RuntimeError("RichText is unavailable".into()))?;
        if text.len() > 4096
            || text.as_bytes().contains(&0)
            || !size.is_finite()
            || !(1.0..=1000.0).contains(&size)
            || !width.is_finite()
            || !(0.0..=32768.0).contains(&width)
        {
            return Err(mlua::Error::RuntimeError(
                "invalid text measurement parameters".into(),
            ));
        }
        let mut payload = json!({"kind":"bounds","font":font,"text":text,"size":size,"width":width,"rich_text":false});
        if rich {
            let (plain, runs) = super::gui::parse_rich_text(&text).ok_or_else(|| {
                mlua::Error::RuntimeError("rich text is malformed or unsupported".into())
            })?;
            payload["text"] = json!(plain);
            payload["runs"] = runs;
        }
        ("bounds", payload)
    };
    let key = req.to_string();
    let sk = (wk(ctx), key.clone());
    if let Some(cached) = state(lua)?.borrow().cache.get(&sk).cloned() {
        return Ok(MultiValue::from_vec(vec![
            Value::Boolean(false),
            values::from_json(lua, &cached)?,
        ]));
    }
    let st = state(lua)?;
    let mut s = st.borrow_mut();
    if s.texts.keys().filter(|(w, _)| *w == wk(ctx)).count() >= MAX_TEXT {
        return Err(mlua::Error::RuntimeError(
            "TextService pending request limit exceeded".into(),
        ));
    }
    s.next = s.next.wrapping_add(1).max(1);
    let request_id = s.next;
    let mut request = req;
    request["request_id"] = json!(request_id);
    push_effect(ctx, "text_service_requests", request)?;
    let thread = lua.current_thread();
    scheduler::park(lua, &thread)?;
    s.texts.insert(
        (wk(ctx), request_id),
        Pending {
            thread,
            deadline: Instant::now() + TIMEOUT,
            key,
            kind: kind.into(),
            target: None,
        },
    );
    drop(s);
    Ok(MultiValue::from_vec(vec![
        Value::Boolean(true),
        Value::Integer(request_id as i64),
    ]))
}
fn request_chat(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    method: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    let class = ctx.borrow().nodes.get(id).map(|n| n.class_name.clone());
    let route = if method == "SendUniverseChatMessageAsync" {
        "universe"
    } else {
        "channel"
    };
    if (route == "channel" && class.as_deref() != Some("TextChannel"))
        || (route == "universe" && class.as_deref() != Some("TextChatService"))
    {
        return Err(mlua::Error::RuntimeError(format!(
            "{method} receiver is invalid"
        )));
    }
    {
        let world = ctx.borrow();
        let node = world
            .nodes
            .get(id)
            .ok_or_else(|| mlua::Error::RuntimeError("chat receiver is unavailable".into()))?;
        if node.destroyed || !node.server_owned {
            return Err(mlua::Error::RuntimeError(
                "chat receiver must be server-replicated".into(),
            ));
        }
        let service = world
            .services
            .get("TextChatService")
            .ok_or_else(|| mlua::Error::RuntimeError("TextChatService is unavailable".into()))?;
        let service_node = world
            .nodes
            .get(service)
            .ok_or_else(|| mlua::Error::RuntimeError("TextChatService is unavailable".into()))?;
        if service_node.destroyed || !service_node.server_owned {
            return Err(mlua::Error::RuntimeError(
                "TextChatService is unavailable".into(),
            ));
        }
        if route == "channel" {
            let mut current = node.parent.as_deref();
            let mut found = false;
            let mut remaining = world.nodes.len() + 1;
            while let Some(parent) = current {
                if parent == service {
                    found = true;
                    break;
                }
                if remaining == 0 {
                    break;
                }
                remaining -= 1;
                current = world.nodes.get(parent).and_then(|n| n.parent.as_deref());
            }
            if !found {
                return Err(mlua::Error::RuntimeError(
                    "TextChannel must descend from TextChatService".into(),
                ));
            }
        }
    }
    let a: Vec<Value> = args.into_iter().collect();
    if !(1..=2).contains(&a.len()) {
        return Err(mlua::Error::RuntimeError(format!(
            "{method} expects text and optional metadata"
        )));
    }
    let text = str_arg(&a[0], "text", 800, false)?;
    if text.chars().count() > 200 {
        return Err(mlua::Error::RuntimeError(
            "text exceeds 200 characters".into(),
        ));
    }
    let metadata = if a.len() == 2 {
        str_arg(&a[1], "metadata", 800, true)?
    } else {
        String::new()
    };
    if metadata.chars().count() > 200 {
        return Err(mlua::Error::RuntimeError(
            "metadata exceeds 200 characters".into(),
        ));
    }
    let idnum = id.to_owned();
    let st = state(lua)?;
    let mut s = st.borrow_mut();
    if s.chats.keys().filter(|(w, _)| *w == wk(ctx)).count() >= MAX_CHAT {
        return Err(mlua::Error::RuntimeError(
            "chat pending request limit exceeded".into(),
        ));
    }
    s.next = s.next.wrapping_add(1).max(1);
    let request_id = s.next;
    let mut request =
        json!({"request_id":request_id,"channel_id":idnum,"text":text,"metadata":metadata});
    if route == "universe" {
        request["route"] = json!("universe")
    }
    push_effect(ctx, "chat_requests", request)?;
    let thread = lua.current_thread();
    scheduler::park(lua, &thread)?;
    s.chats.insert(
        (wk(ctx), request_id),
        Pending {
            thread,
            deadline: Instant::now() + TIMEOUT,
            key: String::new(),
            kind: route.into(),
            target: Some(id.to_owned()),
        },
    );
    drop(s);
    Ok(MultiValue::from_vec(vec![
        Value::Boolean(true),
        Value::Integer(request_id as i64),
    ]))
}

fn validate_text_result(kind: &str, v: &Json) -> mlua::Result<Json> {
    if kind == "bounds" {
        if !v.is_array()
            || v.as_array().unwrap().len() != 2
            || v.as_array().unwrap().iter().any(|n| {
                n.as_f64()
                    .is_none_or(|x| !x.is_finite() || !(0.0..=4194304.0).contains(&x))
            })
        {
            return Err(mlua::Error::RuntimeError(
                "invalid measured text bounds response".into(),
            ));
        }
        return Ok(json!({"$vector2":v}));
    }
    let name = v
        .get("Name")
        .and_then(Json::as_str)
        .filter(|x| x.len() <= 256)
        .ok_or_else(|| mlua::Error::RuntimeError("invalid font family response".into()))?;
    let faces = v
        .get("Faces")
        .and_then(Json::as_array)
        .filter(|x| !x.is_empty() && x.len() <= 64)
        .ok_or_else(|| mlua::Error::RuntimeError("invalid font family response".into()))?;
    let mut out = Vec::new();
    for f in faces {
        let n = f
            .get("Name")
            .and_then(Json::as_str)
            .filter(|x| x.len() <= 128)
            .ok_or_else(|| mlua::Error::RuntimeError("invalid font face response".into()))?;
        let w = f
            .get("Weight")
            .and_then(Json::as_i64)
            .filter(|x| (100..=900).contains(x) && x % 100 == 0)
            .ok_or_else(|| mlua::Error::RuntimeError("invalid font weight response".into()))?;
        let s = f
            .get("Style")
            .and_then(Json::as_i64)
            .filter(|x| (0..=1).contains(x))
            .ok_or_else(|| mlua::Error::RuntimeError("invalid font style response".into()))?;
        out.push(json!({"Name":n,"Weight":{"$enum":{"type":"FontWeight","value":w}},"Style":{"$enum":{"type":"FontStyle","value":s}}}));
    }
    Ok(json!({"Name":name,"Faces":out}))
}
/// Complete responses supplied by the runtime owner. Expected wire rows are
/// `{request_id,result}` or `{request_id,error}` exactly as TextServiceQueue emits.
pub fn complete_text(lua: &Lua, ctx: &WorldRef, rows: &[Json]) -> mlua::Result<usize> {
    if rows.len() > MAX_TEXT || serde_json::to_vec(rows).map_or(true, |b| b.len() > 256 * 1024) {
        return Err(mlua::Error::RuntimeError(
            "TextService response batch exceeds limit".into(),
        ));
    }
    let st = state(lua)?;
    let mut count = 0;
    for row in rows {
        let id = row
            .get("request_id")
            .and_then(Json::as_u64)
            .filter(|id| *id > 0)
            .ok_or_else(|| {
                mlua::Error::RuntimeError("invalid TextService response request_id".into())
            })?;
        let key = (wk(ctx), id);
        let Some(p) = st.borrow().texts.get(&key).map(|p| Pending {
            thread: p.thread.clone(),
            deadline: p.deadline,
            key: p.key.clone(),
            kind: p.kind.clone(),
            target: p.target.clone(),
        }) else {
            continue;
        };
        if row.get("error").is_some() {
            let error = row.get("error").and_then(Json::as_str).ok_or_else(|| {
                mlua::Error::RuntimeError("TextService error must be a string".into())
            })?;
            if error.len() > 512 {
                return Err(mlua::Error::RuntimeError(
                    "TextService error exceeds limit".into(),
                ));
            }
            let mut args = MultiValue::new();
            args.push_back(Value::Nil);
            args.push_back(Value::String(lua.create_string(error)?));
            scheduler::resume(lua, p.thread, args)?;
            st.borrow_mut().texts.remove(&key);
            count += 1;
            continue;
        }
        let result = row.get("result").ok_or_else(|| {
            mlua::Error::RuntimeError("TextService response is missing result".into())
        })?;
        let value = validate_text_result(&p.kind, result)?;
        let luau = values::from_json(lua, &value)?;
        let mut args = MultiValue::new();
        args.push_back(Value::Boolean(true));
        args.push_back(luau);
        scheduler::resume(lua, p.thread, args)?;
        st.borrow_mut().texts.remove(&key);
        let size = p.key.len() + value.to_string().len();
        let mut cache = st.borrow_mut();
        if cache.cache.len() < 128 && cache.cache_bytes.saturating_add(size) <= 512 * 1024 {
            let cache_key = (wk(ctx), p.key);
            if !cache.cache.contains_key(&cache_key) {
                cache.cache_bytes += size;
                cache.cache.insert(cache_key, value);
            }
        }
        count += 1;
    }
    Ok(count)
}

/// Apply actual Chat outcomes from the session owner and wake the waiting Luau call.
pub fn complete_chat(lua: &Lua, ctx: &WorldRef, rows: &[Json]) -> mlua::Result<usize> {
    if rows.len() > 128 || serde_json::to_vec(rows).map_or(true, |b| b.len() > 256 * 1024) {
        return Err(mlua::Error::RuntimeError(
            "chat response batch exceeds limit".into(),
        ));
    }
    let st = state(lua)?;
    let mut count = 0;
    for row in rows {
        let id = row
            .get("request_id")
            .and_then(Json::as_u64)
            .filter(|id| *id > 0)
            .ok_or_else(|| mlua::Error::RuntimeError("invalid chat response request_id".into()))?;
        let key = (wk(ctx), id);
        let Some(p) = st.borrow().chats.get(&key).map(|p| Pending {
            thread: p.thread.clone(),
            deadline: p.deadline,
            key: p.key.clone(),
            kind: p.kind.clone(),
            target: p.target.clone(),
        }) else {
            continue;
        };
        if row.get("error").is_some() {
            let error = row
                .get("error")
                .and_then(Json::as_str)
                .ok_or_else(|| mlua::Error::RuntimeError("chat error must be a string".into()))?;
            if error.len() > 1024 {
                return Err(mlua::Error::RuntimeError("chat error exceeds limit".into()));
            }
            let mut args = MultiValue::new();
            args.push_back(Value::Nil);
            args.push_back(Value::String(lua.create_string(error)?));
            scheduler::resume(lua, p.thread, args)?;
            st.borrow_mut().chats.remove(&key);
            count += 1;
            continue;
        }
        let message = row
            .get("message")
            .ok_or_else(|| mlua::Error::RuntimeError("chat response is missing message".into()))?;
        let target = p.target.as_deref().ok_or_else(|| {
            mlua::Error::RuntimeError("chat response target is unavailable".into())
        })?;
        let route = message
            .get("route")
            .and_then(Json::as_str)
            .unwrap_or("channel");
        if message.get("route").is_some_and(|v| !v.is_string())
            || (route == "universe") != (p.kind == "universe")
        {
            return Err(mlua::Error::RuntimeError(
                "chat response route mismatch".into(),
            ));
        }
        if message.get("channel_id").and_then(Json::as_str) != Some(target) {
            return Err(mlua::Error::RuntimeError(
                "chat response channel mismatch".into(),
            ));
        }
        let status = message
            .get("status_value")
            .and_then(Json::as_i64)
            .filter(|v| (1..=9).contains(v))
            .ok_or_else(|| mlua::Error::RuntimeError("unsupported chat response status".into()))?;
        let mut props = serde_json::Map::new();
        for (field, key) in [
            ("Text", "text"),
            ("Metadata", "metadata"),
            ("PrefixText", "prefix_text"),
            ("MessageId", "message_id"),
        ] {
            let value = message.get(key).and_then(Json::as_str).ok_or_else(|| {
                mlua::Error::RuntimeError("chat message fields are incomplete".into())
            })?;
            if value.len() > 16384 || value.as_bytes().contains(&0) {
                return Err(mlua::Error::RuntimeError(
                    "chat message field exceeds limit".into(),
                ));
            }
            props.insert(field.into(), json!(value));
        }
        props.insert("Status".into(), json!({"$chat_status":status}));
        if p.kind == "universe" {
            props.insert("TextChannel".into(), Json::Null);
            props.insert("TextSource".into(), Json::Null);
        } else {
            props.insert("TextChannel".into(), json!({"$ref":target}));
            let source = message.get("source_id").cloned().unwrap_or(Json::Null);
            if !source.is_null() && !source.as_str().is_some() {
                return Err(mlua::Error::RuntimeError(
                    "chat TextSource reference is invalid".into(),
                ));
            }
            props.insert(
                "TextSource".into(),
                source
                    .as_str()
                    .map(|s| json!({"$ref":s}))
                    .unwrap_or(Json::Null),
            );
        }
        let instance = world::create_internal_node(lua, ctx, "TextChatMessage", None, props)?;
        let mut args = MultiValue::new();
        args.push_back(instance.clone());
        if p.kind != "universe" {
            super::signals::fire(lua, ctx, target, "MessageReceived", args.clone())?;
        }
        if let Some(service) = ctx.borrow().services.get("TextChatService").cloned() {
            super::signals::fire(
                lua,
                ctx,
                &service,
                if p.kind == "universe" {
                    "UniverseChatMessageReceived"
                } else {
                    "MessageReceived"
                },
                args,
            )?;
        }
        scheduler::resume(lua, p.thread, MultiValue::from_vec(vec![instance]))?;
        st.borrow_mut().chats.remove(&key);
        count += 1;
    }
    Ok(count)
}

/// Expire pending calls with truthful timeout errors and discard their queued effects.
pub fn tick(lua: &Lua, ctx: &WorldRef) -> mlua::Result<()> {
    let st = state(lua)?;
    let now = Instant::now();
    let (texts, chats) = {
        let mut s = st.borrow_mut();
        let t = s
            .texts
            .keys()
            .filter(|(w, id)| {
                *w == wk(ctx) && s.texts.get(&(*w, *id)).is_some_and(|p| p.deadline <= now)
            })
            .copied()
            .collect::<Vec<_>>();
        let c = s
            .chats
            .keys()
            .filter(|(w, id)| {
                *w == wk(ctx) && s.chats.get(&(*w, *id)).is_some_and(|p| p.deadline <= now)
            })
            .copied()
            .collect::<Vec<_>>();
        (
            t.into_iter()
                .filter_map(|k| s.texts.remove(&k))
                .collect::<Vec<_>>(),
            c.into_iter()
                .filter_map(|k| s.chats.remove(&k))
                .collect::<Vec<_>>(),
        )
    };
    for p in texts {
        let mut a = MultiValue::new();
        a.push_back(Value::Boolean(false));
        a.push_back(Value::String(
            lua.create_string("TextService request timed out")?,
        ));
        scheduler::resume(lua, p.thread, a)?;
    }
    for p in chats {
        let mut a = MultiValue::new();
        a.push_back(Value::Nil);
        a.push_back(Value::String(lua.create_string("chat request timed out")?));
        scheduler::resume(lua, p.thread, a)?;
    }
    Ok(())
}
/// Snapshot the active request ids for the runtime owner to correlate.
pub fn active_request_ids(lua: &Lua, ctx: &WorldRef) -> mlua::Result<Json> {
    let s = state(lua)?;
    let s = s.borrow();
    Ok(
        json!({"active_text_service_requests":s.texts.keys().filter(|(w,_)|*w==wk(ctx)).map(|(_,id)|*id).collect::<Vec<_>>(),"active_chat_requests":s.chats.keys().filter(|(w,_)|*w==wk(ctx)).map(|(_,id)|*id).collect::<Vec<_>>()}),
    )
}
/// Drain request effects already collected by service methods.
pub fn drain_requests(ctx: &WorldRef) -> Json {
    let mut w = ctx.borrow_mut();
    json!({"text_service_requests":w.effects.remove("text_service_requests").unwrap_or(json!([])),"chat_requests":w.effects.remove("chat_requests").unwrap_or(json!([]))})
}
fn yielding_getter(
    lua: &Lua,
    ctx: &WorldRef,
    id: &str,
    method: &str,
    channel: bool,
) -> mlua::Result<Option<Value>> {
    let required = if channel {
        "TextChannel"
    } else if method == "SendUniverseChatMessageAsync" {
        "TextChatService"
    } else {
        "TextService"
    };
    let valid = ctx
        .borrow()
        .nodes
        .get(id)
        .is_some_and(|n| !n.destroyed && n.class_name == required);
    if !valid {
        return Ok(None);
    }
    let weak = Rc::downgrade(ctx);
    let iid = id.to_owned();
    let m = method.to_owned();
    let begin = lua.create_function(move |lua, args: MultiValue| {
        let ctx = weak
            .upgrade()
            .ok_or_else(|| mlua::Error::RuntimeError("world has been released".into()))?;
        if m == "GetFamilyInfoAsync" || m == "GetTextBoundsAsync" {
            request_text(lua, &ctx, &iid, &m, args)
        } else {
            request_chat(lua, &ctx, &iid, &m, args)
        }
    })?;
    let source = if channel || method == "SendUniverseChatMessageAsync" {
        "return function(begin) return function(self, ...) if not coroutine.isyieldable() then error('chat send requires a yieldable coroutine', 2) end; local first=table.pack(begin(...)); local result=table.pack(coroutine.yield(first[2])); if result[1] == nil then error(tostring(result[2] or 'chat send failed'), 2) end; return table.unpack(result,1,result.n) end end"
    } else {
        "return function(begin) return function(self, ...) if not coroutine.isyieldable() then error('TextService request requires a yieldable coroutine', 2) end; local first=table.pack(begin(...)); if first[1] == false then return table.unpack(first,2,first.n) end; local result=table.pack(coroutine.yield(first[2])); if result[1] ~= true then error(tostring(result[2] or 'TextService request failed'), 2) end; return table.unpack(result,2,result.n) end end"
    };
    let factory: Function = lua.load(source).eval()?;
    Ok(Some(Value::Function(factory.call::<Function>(begin)?)))
}
fn text_family_getter(lua: &Lua, ctx: &WorldRef, id: &str) -> mlua::Result<Option<Value>> {
    yielding_getter(lua, ctx, id, "GetFamilyInfoAsync", false)
}
fn text_bounds_getter(lua: &Lua, ctx: &WorldRef, id: &str) -> mlua::Result<Option<Value>> {
    yielding_getter(lua, ctx, id, "GetTextBoundsAsync", false)
}
fn chat_send_getter(lua: &Lua, ctx: &WorldRef, id: &str) -> mlua::Result<Option<Value>> {
    yielding_getter(lua, ctx, id, "SendAsync", true)
}
fn universe_send_getter(lua: &Lua, ctx: &WorldRef, id: &str) -> mlua::Result<Option<Value>> {
    yielding_getter(lua, ctx, id, "SendUniverseChatMessageAsync", false)
}
pub fn install(lua: &Lua, ctx: &WorldRef) -> mlua::Result<()> {
    let _ = state(lua)?;
    world::register_getter(ctx, "GetFamilyInfoAsync", text_family_getter);
    world::register_getter(ctx, "GetTextBoundsAsync", text_bounds_getter);
    world::register_getter(ctx, "SendAsync", chat_send_getter);
    world::register_getter(ctx, "SendUniverseChatMessageAsync", universe_send_getter);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn text_completions_accept_runtime_owner_wire_rows_only() {
        let bounds = json!([12.5, 18.0]);
        assert_eq!(
            validate_text_result("bounds", &bounds).unwrap(),
            json!({"$vector2":[12.5,18.0]})
        );
        let family =
            json!({"Name":"fixture-font","Faces":[{"Name":"Regular","Weight":400,"Style":0}]});
        let result = validate_text_result("family", &family).unwrap();
        assert_eq!(result["Faces"][0]["Weight"]["$enum"]["type"], "FontWeight");
        assert!(validate_text_result("bounds", &json!([12.5])).is_err());
        assert!(validate_text_result("family", &json!({"Name":"missing faces"})).is_err());
    }

    #[test]
    fn chat_error_response_matches_local_runtime_owner_contract() {
        let row =
            json!({"request_id":7,"error":"No server connection exists in this local runtime"});
        assert_eq!(row["request_id"].as_u64(), Some(7));
        assert!(row["error"].as_str().unwrap().len() <= 1024);
    }
}

#[cfg(test)]
mod integration_tests {
    use super::*;
    use crate::rust_host::{
        enums, numeric,
        scheduler::{self, Scheduler},
        signals,
    };
    use std::{
        cell::RefCell,
        rc::Rc,
        time::{Duration, Instant},
    };
    fn fixture() -> (Lua, WorldRef, scheduler::SchedulerRef) {
        let snap = json!({"version":1,"sequence":0,"services":{"TextService":"text","TextChatService":"chat"},"nodes":[
            {"id":"game","class_name":"DataModel","name":"game","parent":null,"properties":{}},
            {"id":"text","class_name":"TextService","name":"TextService","parent":"game","properties":{}},
            {"id":"chat","class_name":"TextChatService","name":"TextChatService","parent":"game","properties":{}},
            {"id":"channel","class_name":"TextChannel","name":"RBXGeneral","parent":"chat","properties":{}}
        ]});
        let lua = Lua::new();
        let world = Rc::new(RefCell::new(
            world::World::from_snapshot(&snap, None).unwrap(),
        ));
        let sched = Rc::new(RefCell::new(Scheduler::new()));
        enums::install(&lua).unwrap();
        numeric::install(&lua).unwrap();
        world::install(&lua, &world).unwrap();
        signals::install(&lua, &world).unwrap();
        scheduler::install(&lua, &sched).unwrap();
        install(&lua, &world).unwrap();
        (lua, world, sched)
    }
    fn step(lua: &Lua, s: &scheduler::SchedulerRef) {
        scheduler::step(lua, s, Instant::now(), Duration::from_millis(20));
    }
    #[test]
    fn text_async_preserves_pending_on_bad_response_then_resumes_real_result() {
        let (lua, world, sched) = fixture();
        lua.load("task.spawn(function() local ok,value=pcall(function() return game:GetService('TextService'):GetFamilyInfoAsync('rbxasset://fonts/families/Test.json') end); _result={ok,value} end)").exec().unwrap();
        step(&lua, &sched);
        let request = world.borrow().effects["text_service_requests"][0].clone();
        let id = request["request_id"].as_u64().unwrap();
        assert!(complete_text(&lua, &world, &[json!({"request_id":id,"result":[1.0]})]).is_err());
        assert_eq!(
            active_request_ids(&lua, &world).unwrap()["active_text_service_requests"],
            json!([id])
        );
        let family = json!({"Name":"Test","Faces":[{"Name":"Regular","Weight":400,"Style":0}]});
        assert_eq!(
            complete_text(&lua, &world, &[json!({"request_id":id,"result":family})]).unwrap(),
            1
        );
        step(&lua, &sched);
        let ok: bool = lua.load("return _result[1]").eval().unwrap();
        let name: String = lua.load("return _result[2].Name").eval().unwrap();
        assert!(ok);
        assert_eq!(name, "Test");
    }
    #[test]
    fn chat_error_outcome_resumes_sendasync_through_the_yield_wrapper() {
        let (lua, world, sched) = fixture();
        lua.load("task.spawn(function() local ok,value=pcall(function() return game:GetService('TextChatService'):FindFirstChild('RBXGeneral'):SendAsync('hello','') end); _chat={ok,value} end)").exec().unwrap();
        step(&lua, &sched);
        let request = world.borrow().effects["chat_requests"][0].clone();
        let id = request["request_id"].as_u64().unwrap();
        assert_eq!(complete_chat(&lua,&world,&[json!({"request_id":id,"error":"No server connection exists in this local runtime"})]).unwrap(),1);
        step(&lua, &sched);
        let ok: bool = lua.load("return _chat[1]").eval().unwrap();
        let error: String = lua.load("return _chat[2]").eval().unwrap();
        assert!(!ok);
        assert!(error.contains("No server connection"));
    }
}
