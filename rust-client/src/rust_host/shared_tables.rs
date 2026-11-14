//! Session and actor-pool SharedTable storage.
//!
//! Lua userdata is only a lightweight handle. Values and table identity live in
//! Arc/Mutex records shared by every VM attached to the same registry, and no
//! lock is held while invoking a Luau callback.

use mlua::{
    AnyUserData, Lua, MetaMethod, MultiValue, Table, UserData, UserDataFields, UserDataMethods,
    Value, Variadic, Vector,
};
use serde_json::{json, Value as Json};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, MutexGuard},
};

const MAX_OBJECTS: usize = 65_536;
const MAX_ENTRIES: usize = 65_536;
const MAX_STRING_BYTES: usize = 1024 * 1024;
const MAX_POOL_BYTES: usize = 64 * 1024 * 1024;
const DATA_OVERHEAD: usize = 256;
const MAX_NESTING: usize = 64;

type SharedRef = Arc<SharedData>;

#[derive(Clone, Debug, Eq)]
enum Key {
    Numeric(u32),
    Text(Vec<u8>),
}
impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Numeric(a), Self::Numeric(b)) => a == b,
            (Self::Text(a), Self::Text(b)) => a == b,
            _ => false,
        }
    }
}
impl PartialOrd for Key {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Key {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (Self::Text(a), Self::Text(b)) => a.cmp(b),
            (Self::Numeric(a), Self::Numeric(b)) => a.cmp(b),
            (Self::Text(_), Self::Numeric(_)) => std::cmp::Ordering::Less,
            (Self::Numeric(_), Self::Text(_)) => std::cmp::Ordering::Greater,
        }
    }
}

#[derive(Clone)]
enum Stored {
    Boolean(bool),
    Number(f64),
    String(Vec<u8>),
    Vector([f32; 3]),
    Json(Json),
    Table(SharedRef),
}
pub struct SharedData {
    id: u64,
    inner: Mutex<TableData>,
}
struct TableData {
    entries: BTreeMap<Key, Stored>,
    frozen: bool,
    revision: u64,
    accounted: usize,
}
struct RegistryInner {
    next_id: u64,
    objects: HashMap<u64, SharedRef>,
    names: HashMap<Vec<u8>, SharedRef>,
    used_bytes: usize,
    names_bytes: usize,
}
impl Drop for RegistryInner {
    fn drop(&mut self) {
        for data in self.objects.values() {
            if let Ok(mut inner) = data.inner.lock() {
                inner.entries.clear();
                inner.accounted = 0;
            }
        }
    }
}

/// Share this value between the root Luau VM and its actor VMs.
#[derive(Clone)]
pub struct SharedTableRegistry {
    inner: Arc<Mutex<RegistryInner>>,
}
/// Strong handle to one shared object, suitable for trusted host bindings.
pub type SharedTableRef = Arc<SharedData>;

impl Default for SharedTableRegistry {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(RegistryInner {
                next_id: 1,
                objects: HashMap::new(),
                names: HashMap::new(),
                used_bytes: 0,
                names_bytes: 0,
            })),
        }
    }
}
impl SharedTableRegistry {
    pub fn create(&self) -> mlua::Result<SharedTableRef> {
        self.create_inner().map_err(err)
    }
    fn create_inner(&self) -> Result<SharedRef, String> {
        let mut p = self.lock_pool()?;
        if p.objects.len() >= MAX_OBJECTS {
            return Err("SharedTable session object limit exceeded".into());
        }
        if p.used_bytes > MAX_POOL_BYTES - DATA_OVERHEAD {
            return Err("SharedTable session memory limit exceeded".into());
        }
        let id = p.next_id;
        p.next_id = p
            .next_id
            .checked_add(1)
            .ok_or("SharedTable session id exhausted")?;
        if id == 0 {
            return Err("SharedTable session id exhausted".into());
        }
        let data = Arc::new(SharedData {
            id,
            inner: Mutex::new(TableData {
                entries: BTreeMap::new(),
                frozen: false,
                revision: 0,
                accounted: 0,
            }),
        });
        p.used_bytes += DATA_OVERHEAD;
        p.objects.insert(id, data.clone());
        Ok(data)
    }
    pub fn find(&self, id: u64) -> Option<SharedTableRef> {
        self.inner.lock().ok()?.objects.get(&id).cloned()
    }
    pub fn contains(&self, data: &SharedTableRef) -> bool {
        self.find(data.id)
            .is_some_and(|found| Arc::ptr_eq(&found, data))
    }
    pub fn get_registered(&self, name: &str) -> mlua::Result<SharedTableRef> {
        self.get_registered_bytes(name.as_bytes())
    }
    fn get_registered_bytes(&self, name: &[u8]) -> mlua::Result<SharedTableRef> {
        if name.len() > MAX_STRING_BYTES {
            return Err(err("SharedTableRegistry name limit exceeded"));
        }
        let mut p = self.lock_pool().map_err(err)?;
        if let Some(found) = p.names.get(name) {
            return Ok(found.clone());
        }
        if p.objects.len() >= MAX_OBJECTS {
            return Err(err("SharedTable session object limit exceeded"));
        }
        let cost = std::mem::size_of::<String>() + name.len() + 32;
        if p.used_bytes > MAX_POOL_BYTES - DATA_OVERHEAD - cost {
            return Err(err("SharedTable session memory limit exceeded"));
        }
        let id = p.next_id;
        p.next_id = p
            .next_id
            .checked_add(1)
            .ok_or_else(|| err("SharedTable session id exhausted"))?;
        if id == 0 {
            return Err(err("SharedTable session id exhausted"));
        }
        let data = Arc::new(SharedData {
            id,
            inner: Mutex::new(TableData {
                entries: BTreeMap::new(),
                frozen: false,
                revision: 0,
                accounted: 0,
            }),
        });
        p.used_bytes += DATA_OVERHEAD + cost;
        p.names_bytes += cost;
        p.objects.insert(id, data.clone());
        p.names.insert(name.to_vec(), data.clone());
        Ok(data)
    }
    pub fn set_registered(&self, name: &str, data: Option<&SharedTableRef>) -> mlua::Result<()> {
        self.set_registered_bytes(name.as_bytes(), data)
    }
    fn set_registered_bytes(&self, name: &[u8], data: Option<&SharedRef>) -> mlua::Result<()> {
        if name.len() > MAX_STRING_BYTES {
            return Err(err("SharedTableRegistry name limit exceeded"));
        }
        let mut p = self.lock_pool().map_err(err)?;
        let old = p.names.get(name).cloned();
        if let Some(value) = data {
            if !p
                .objects
                .get(&value.id)
                .is_some_and(|v| Arc::ptr_eq(v, value))
            {
                return Err(err("SharedTable belongs to a different session pool"));
            }
        }
        let old_cost = if old.is_some() {
            std::mem::size_of::<String>() + name.len() + 32
        } else {
            0
        };
        let new_cost = if data.is_some() {
            std::mem::size_of::<String>() + name.len() + 32
        } else {
            0
        };
        if p.used_bytes - old_cost > MAX_POOL_BYTES - new_cost {
            return Err(err("SharedTable session memory limit exceeded"));
        }
        p.used_bytes = p.used_bytes - old_cost + new_cost;
        p.names_bytes = p.names_bytes - old_cost + new_cost;
        match data {
            Some(value) => {
                p.names.insert(name.to_vec(), value.clone());
            }
            None => {
                p.names.remove(name);
            }
        }
        Ok(())
    }
    fn lock_pool(&self) -> Result<MutexGuard<'_, RegistryInner>, String> {
        self.inner
            .lock()
            .map_err(|_| "SharedTable registry lock poisoned".to_owned())
    }
    fn adjust_memory(&self, add: usize, release: usize) -> mlua::Result<()> {
        let mut p = self.lock_pool().map_err(err)?;
        if release > p.used_bytes {
            return Err(err("SharedTable session memory accounting error"));
        }
        let next = p.used_bytes - release;
        if add > MAX_POOL_BYTES - next {
            return Err(err("SharedTable session memory limit exceeded"));
        }
        p.used_bytes = next + add;
        Ok(())
    }
}

#[derive(Clone)]
struct SharedTableHandle {
    data: SharedRef,
    registry: SharedTableRegistry,
}
fn err(s: impl Into<String>) -> mlua::Error {
    mlua::Error::RuntimeError(s.into())
}
fn lock(data: &SharedData) -> mlua::Result<std::sync::MutexGuard<'_, TableData>> {
    data.inner
        .lock()
        .map_err(|_| err("SharedTable lock poisoned"))
}
fn memory(key: &Key, value: &Stored) -> usize {
    let key_bytes = match key {
        Key::Numeric(_) => 0,
        Key::Text(s) => s.len(),
    };
    let value_bytes = match value {
        Stored::String(s) => std::mem::size_of::<Stored>() + s.len(),
        Stored::Json(v) => {
            std::mem::size_of::<Stored>()
                + serde_json::to_vec(v)
                    .map(|v| v.len())
                    .unwrap_or(usize::MAX / 2)
        }
        _ => std::mem::size_of::<Stored>(),
    };
    std::mem::size_of::<Key>() + 32 + key_bytes + value_bytes
}
fn mutable(data: &TableData) -> mlua::Result<()> {
    if data.frozen {
        Err(err("attempt to modify a frozen SharedTable"))
    } else {
        Ok(())
    }
}
fn set_locked(
    registry: &SharedTableRegistry,
    data: &mut TableData,
    key: Key,
    value: Option<Stored>,
) -> mlua::Result<()> {
    let before = data.entries.get(&key).map(|v| memory(&key, v)).unwrap_or(0);
    let after = value.as_ref().map(|v| memory(&key, v)).unwrap_or(0);
    if value.is_some() && !data.entries.contains_key(&key) && data.entries.len() >= MAX_ENTRIES {
        return Err(err("SharedTable entry limit exceeded"));
    }
    registry.adjust_memory(after, before)?;
    if let Some(v) = value {
        data.entries.insert(key, v);
    } else {
        data.entries.remove(&key);
    }
    data.accounted = data.accounted.saturating_sub(before).saturating_add(after);
    data.revision = data.revision.wrapping_add(1);
    Ok(())
}
fn parse_key(v: &Value) -> mlua::Result<Key> {
    match v {
        Value::String(s) => {
            let b = s.as_bytes();
            if b.len() > MAX_STRING_BYTES {
                return Err(err("SharedTable key string limit exceeded"));
            }
            Ok(Key::Text(b.to_vec()))
        }
        Value::Integer(n) if *n >= 0 && *n <= u32::MAX as i64 => Ok(Key::Numeric(*n as u32)),
        Value::Number(n) if n.is_finite() && *n >= 0. && *n < 4_294_967_296. && n.fract() == 0. => {
            Ok(Key::Numeric(*n as u32))
        }
        _ => Err(err("SharedTable keys must be strings or uint32 integers")),
    }
}
fn key_value(lua: &Lua, k: &Key) -> mlua::Result<Value> {
    Ok(match k {
        Key::Numeric(n) => Value::Integer(*n as i64),
        Key::Text(s) => Value::String(lua.create_string(s)?),
    })
}
fn push_value(lua: &Lua, registry: &SharedTableRegistry, v: &Stored) -> mlua::Result<Value> {
    match v {
        Stored::Boolean(v) => Ok(Value::Boolean(*v)),
        Stored::Number(v) => Ok(Value::Number(*v)),
        Stored::String(v) => Ok(Value::String(lua.create_string(v)?)),
        Stored::Vector(v) => Ok(Value::Vector(Vector::new(v[0], v[1], v[2]))),
        Stored::Json(j) => super::values::from_json(lua, j),
        Stored::Table(data) => Ok(Value::UserData(lua.create_userdata(SharedTableHandle {
            data: data.clone(),
            registry: registry.clone(),
        })?)),
    }
}

fn encode_atomic(
    lua: &Lua,
    registry: &SharedTableRegistry,
    value: &Value,
    seen: Option<&mut HashMap<usize, SharedRef>>,
    depth: usize,
) -> mlua::Result<Stored> {
    if depth > MAX_NESTING {
        return Err(err("SharedTable nested value limit exceeded"));
    }
    match value {
        Value::Boolean(v) => Ok(Stored::Boolean(*v)),
        Value::Integer(v) => Ok(Stored::Number(*v as f64)),
        Value::Number(v) => Ok(Stored::Number(*v)),
        Value::String(v) => {
            let b = v.as_bytes();
            if b.len() > MAX_STRING_BYTES {
                return Err(err("SharedTable string limit exceeded"));
            }
            Ok(Stored::String(b.to_vec()))
        }
        Value::Vector(v) => Ok(Stored::Vector([v.x(), v.y(), v.z()])),
        Value::UserData(ud) => {
            if let Ok(h) = ud.borrow::<SharedTableHandle>() {
                if !registry.contains(&h.data) {
                    return Err(err("SharedTable belongs to a different session pool"));
                }
                return Ok(Stored::Table(h.data.clone()));
            }
            if super::world::instance_id(value).is_some() {
                return Err(err("Instances cannot be stored in a SharedTable"));
            }
            let j = super::values::to_json(lua, value)?;
            let key = j
                .as_object()
                .and_then(|o| (o.len() == 1).then(|| o.keys().next().cloned()).flatten())
                .ok_or_else(|| {
                    err("userdata is not an allowed serializable SharedTable datatype")
                })?;
            if !allowed_codec(&key) {
                return Err(err(
                    "userdata is not an allowed serializable SharedTable datatype",
                ));
            }
            Ok(Stored::Json(j))
        }
        Value::Table(t) => {
            let Some(seen) = seen else {
                return Err(err(
                    "plain tables are supported only in SharedTable.new(table)",
                ));
            };
            let id = t.to_pointer() as usize;
            if let Some(existing) = seen.get(&id) {
                return Ok(Stored::Table(existing.clone()));
            }
            let data = registry.create()?;
            seen.insert(id, data.clone());
            fill_from_table(lua, registry, &data, t, seen, depth + 1)?;
            Ok(Stored::Table(data))
        }
        _ => Err(err("unsupported SharedTable value type")),
    }
}
fn allowed_codec(key: &str) -> bool {
    matches!(
        key,
        "$enum"
            | "$cframe"
            | "$catalog_search_params"
            | "$path_waypoint"
            | "$font"
            | "$axes"
            | "$faces"
            | "$vector2int16"
            | "$vector3int16"
            | "$region3int16"
            | "$ray"
            | "$color3"
            | "$vector2"
            | "$udim"
            | "$udim2"
            | "$number_range"
            | "$rect"
            | "$brickcolor"
            | "$datetime"
            | "$number_keypoint"
            | "$number_sequence"
            | "$color_keypoint"
            | "$color_sequence"
            | "$physical_properties"
            | "$rotation_curve_key"
            | "$security_capabilities"
            | "$content"
    )
}
fn fill_from_table(
    lua: &Lua,
    registry: &SharedTableRegistry,
    data: &SharedRef,
    t: &Table,
    seen: &mut HashMap<usize, SharedRef>,
    depth: usize,
) -> mlua::Result<()> {
    if depth > MAX_NESTING {
        return Err(err("SharedTable nested value limit exceeded"));
    }
    for pair in t.clone().pairs::<Value, Value>() {
        let (k, v) = pair?;
        let key = parse_key(&k).map_err(|_| err("invalid SharedTable table key"))?;
        let stored = encode_atomic(lua, registry, &v, Some(seen), depth)?;
        let mut inner = lock(data)?;
        mutable(&inner)?;
        set_locked(registry, &mut inner, key, Some(stored))?;
    }
    Ok(())
}
fn capture_table(lua: &Lua, registry: &SharedTableRegistry, t: &Table) -> mlua::Result<SharedRef> {
    let data = registry.create()?;
    let mut seen = HashMap::new();
    seen.insert(t.to_pointer() as usize, data.clone());
    fill_from_table(lua, registry, &data, t, &mut seen, 0)?;
    Ok(data)
}
fn wrapper_value(
    lua: &Lua,
    registry: &SharedTableRegistry,
    data: SharedRef,
) -> mlua::Result<Value> {
    if !registry.contains(&data) {
        return Err(err("SharedTable belongs to a different session pool"));
    }
    Ok(Value::UserData(lua.create_userdata(SharedTableHandle {
        data,
        registry: registry.clone(),
    })?))
}

impl UserData for SharedTableHandle {
    fn add_fields<F: UserDataFields<Self>>(f: &mut F) {
        f.add_meta_field("__type", "SharedTable");
    }
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_meta_method(MetaMethod::Index, |lua, this, key: Value| {
            let key = parse_key(&key)?;
            let value = { lock(&this.data)?.entries.get(&key).cloned() };
            match value {
                Some(v) => push_value(lua, &this.registry, &v),
                None => Ok(Value::Nil),
            }
        });
        m.add_meta_method(
            MetaMethod::NewIndex,
            |lua, this, (key, value): (Value, Value)| {
                let key = parse_key(&key)?;
                let stored = if matches!(value, Value::Nil) {
                    None
                } else {
                    Some(encode_atomic(lua, &this.registry, &value, None, 0)?)
                };
                let mut inner = lock(&this.data)?;
                mutable(&inner)?;
                set_locked(&this.registry, &mut inner, key, stored)
            },
        );
        m.add_meta_method(MetaMethod::Eq, |_, a, b: AnyUserData| {
            Ok(b.borrow::<SharedTableHandle>()
                .map(|b| Arc::ptr_eq(&a.data, &b.data))
                .unwrap_or(false))
        });
        m.add_meta_method(MetaMethod::Iter, |lua, this, ()| {
            let snapshot = {
                lock(&this.data)?
                    .entries
                    .iter()
                    .map(|(k, v)| Ok((key_value(lua, k)?, push_value(lua, &this.registry, v)?)))
                    .collect::<mlua::Result<Vec<_>>>()?
            };
            let pos = Arc::new(Mutex::new(0usize));
            let iter = lua.create_function(move |_, _: MultiValue| {
                let mut i = pos
                    .lock()
                    .map_err(|_| err("SharedTable iterator lock poisoned"))?;
                if *i >= snapshot.len() {
                    return Ok((Value::Nil, Value::Nil));
                }
                let pair = snapshot[*i].clone();
                *i += 1;
                Ok(pair)
            })?;
            Ok((iter, Value::Nil, Value::Nil))
        });
        m.add_method("clear", |_, this, ()| {
            let mut inner = lock(&this.data)?;
            mutable(&inner)?;
            let release = inner.accounted;
            this.registry.adjust_memory(0, release)?;
            inner.accounted = 0;
            inner.entries.clear();
            inner.revision = inner.revision.wrapping_add(1);
            Ok(())
        });
        m.add_method("size", |_, this, ()| Ok(lock(&this.data)?.entries.len()));
        m.add_method("isFrozen", |_, this, ()| Ok(lock(&this.data)?.frozen));
        m.add_method("increment", |_, this, (key, delta): (Value, f64)| {
            let key = parse_key(&key)?;
            let mut inner = lock(&this.data)?;
            mutable(&inner)?;
            let Some(Stored::Number(current)) = inner.entries.get(&key) else {
                return Err(err("increment key must exist and contain a number"));
            };
            let original = *current;
            inner.entries.insert(key, Stored::Number(original + delta));
            inner.revision = inner.revision.wrapping_add(1);
            Ok(original)
        });
        m.add_method(
            "update",
            |lua, this, (key, callback): (Value, mlua::Function)| {
                let key = parse_key(&key)?;
                for _ in 0..64 {
                    let (old, rev) = {
                        let inner = lock(&this.data)?;
                        mutable(&inner)?;
                        (inner.entries.get(&key).cloned(), inner.revision)
                    };
                    let old = match old {
                        Some(v) => push_value(lua, &this.registry, &v)?,
                        None => Value::Nil,
                    };
                    let next: Value = callback.call(old)?;
                    let stored = if matches!(next, Value::Nil) {
                        None
                    } else {
                        Some(encode_atomic(lua, &this.registry, &next, None, 0)?)
                    };
                    let mut inner = lock(&this.data)?;
                    mutable(&inner)?;
                    if inner.revision != rev {
                        continue;
                    }
                    set_locked(&this.registry, &mut inner, key, stored)?;
                    return Ok(());
                }
                Err(err("concurrent SharedTable update retry limit exceeded"))
            },
        );
        m.add_method("clone", |lua, this, deep: Option<bool>| {
            clone_value(lua, this, deep.unwrap_or(false), false)
        });
        m.add_method("cloneAndFreeze", |lua, this, deep: Option<bool>| {
            clone_value(lua, this, deep.unwrap_or(false), true)
        });
    }
}
fn clone_data(
    registry: &SharedTableRegistry,
    source: &SharedRef,
    deep: bool,
    frozen: bool,
    seen: &mut HashMap<u64, SharedRef>,
    depth: usize,
) -> mlua::Result<SharedRef> {
    if depth > MAX_NESTING {
        return Err(err("SharedTable clone nesting limit exceeded"));
    }
    if deep {
        if let Some(found) = seen.get(&source.id) {
            return Ok(found.clone());
        }
    }
    let dest = registry.create()?;
    if deep {
        seen.insert(source.id, dest.clone());
    }
    let values = lock(source)?.entries.clone();
    for (k, v) in values {
        let v = match v {
            Stored::Table(child) if deep => {
                Stored::Table(clone_data(registry, &child, true, frozen, seen, depth + 1)?)
            }
            x => x,
        };
        let mut d = lock(&dest)?;
        set_locked(registry, &mut d, k, Some(v))?;
    }
    if frozen {
        lock(&dest)?.frozen = true
    }
    Ok(dest)
}
fn clone_value(
    lua: &Lua,
    this: &SharedTableHandle,
    deep: bool,
    frozen: bool,
) -> mlua::Result<Value> {
    let data = clone_data(
        &this.registry,
        &this.data,
        deep,
        frozen,
        &mut HashMap::new(),
        0,
    )?;
    wrapper_value(lua, &this.registry, data)
}

fn decode_table(lua: &Lua, j: &Json) -> mlua::Result<Value> {
    if j.as_object().is_none_or(|o| o.len() != 1) {
        return Err(err("invalid $shared_table_ref"));
    }
    let id = j
        .get("$shared_table_ref")
        .and_then(Json::as_u64)
        .ok_or_else(|| err("invalid $shared_table_ref"))?;
    let registry = lua
        .app_data_ref::<SharedTableRegistry>()
        .ok_or_else(|| err("SharedTable session registry is missing"))?
        .clone();
    let data = registry
        .find(id)
        .ok_or_else(|| err("SharedTable reference is outside this session pool"))?;
    wrapper_value(lua, &registry, data)
}
fn encode_table(u: &AnyUserData) -> mlua::Result<Option<Json>> {
    let Ok(h) = u.borrow::<SharedTableHandle>() else {
        return Ok(None);
    };
    if !h.registry.contains(&h.data) {
        return Err(err("SharedTable belongs to a different session pool"));
    }
    Ok(Some(json!({"$shared_table_ref":h.data.id})))
}

pub fn install(
    lua: &Lua,
    ctx: &super::world::WorldRef,
    registry: SharedTableRegistry,
) -> mlua::Result<()> {
    lua.set_app_data(registry.clone());
    let lib = lua.create_table()?;
    lib.set(
        "new",
        lua.create_function({
            let registry = registry.clone();
            move |lua, args: Variadic<Value>| {
                let data = match args.as_slice() {
                    [] => registry.create()?,
                    [Value::Table(t)] => capture_table(lua, &registry, t)?,
                    _ => return Err(err("SharedTable.new expects an optional table")),
                };
                wrapper_value(lua, &registry, data)
            }
        })?,
    )?;
    lib.set_readonly(true);
    lua.globals().set("SharedTable", lib)?;
    super::values::register_codec(
        lua,
        super::values::Codec {
            key: "$shared_table_ref",
            type_name: "SharedTable",
            decode: decode_table,
            encode: encode_table,
        },
    )?;
    super::world::register_method(ctx, "SharedTableRegistry", "GetSharedTable", registry_get);
    super::world::register_method(ctx, "SharedTableRegistry", "SetSharedTable", registry_set);
    Ok(())
}
fn registry_from_lua(lua: &Lua) -> mlua::Result<SharedTableRegistry> {
    lua.app_data_ref::<SharedTableRegistry>()
        .map(|r| r.clone())
        .ok_or_else(|| err("SharedTable session registry is missing"))
}
fn registry_get(
    lua: &Lua,
    ctx: &super::world::WorldRef,
    id: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    ensure_service(ctx, id)?;
    let args: Vec<Value> = args.into_iter().collect();
    let Some(Value::String(name)) = args.first() else {
        return Err(err("GetSharedTable expects a string name"));
    };
    let name = name.as_bytes().to_vec();
    let pool = registry_from_lua(lua)?;
    let data = pool.get_registered_bytes(&name)?;
    let mut out = MultiValue::new();
    out.push_back(wrapper_value(lua, &pool, data)?);
    Ok(out)
}
fn registry_set(
    lua: &Lua,
    ctx: &super::world::WorldRef,
    id: &str,
    args: MultiValue,
) -> mlua::Result<MultiValue> {
    ensure_service(ctx, id)?;
    let args: Vec<Value> = args.into_iter().collect();
    let Some(Value::String(name)) = args.first() else {
        return Err(err("SetSharedTable expects a string name"));
    };
    let name = name.as_bytes().to_vec();
    let value = args.get(1).cloned().unwrap_or(Value::Nil);
    let pool = registry_from_lua(lua)?;
    let data = if matches!(value, Value::Nil) {
        None
    } else if let Value::UserData(u) = value {
        let h = u
            .borrow::<SharedTableHandle>()
            .map_err(|_| err("SetSharedTable expects a SharedTable"))?;
        if !pool.contains(&h.data) {
            return Err(err("SharedTable belongs to a different session pool"));
        }
        Some(h.data.clone())
    } else {
        return Err(err("SetSharedTable expects a SharedTable or nil"));
    };
    pool.set_registered_bytes(&name, data.as_ref())?;
    Ok(MultiValue::new())
}
fn ensure_service(ctx: &super::world::WorldRef, id: &str) -> mlua::Result<()> {
    let w = ctx.borrow();
    let n = w
        .nodes
        .get(id)
        .ok_or_else(|| err("SharedTableRegistry is not in this world"))?;
    if n.class_name != "SharedTableRegistry" {
        return Err(err(
            "SharedTableRegistry method requires SharedTableRegistry",
        ));
    }
    Ok(())
}

pub fn from_userdata(
    value: &Value,
    registry: &SharedTableRegistry,
) -> mlua::Result<SharedTableRef> {
    let Value::UserData(ud) = value else {
        return Err(err("SharedTable expected"));
    };
    let h = ud.borrow::<SharedTableHandle>()?;
    if !registry.contains(&h.data) {
        return Err(err("SharedTable belongs to a different session pool"));
    }
    Ok(h.data.clone())
}
pub fn userdata(
    lua: &Lua,
    data: SharedTableRef,
    registry: &SharedTableRegistry,
) -> mlua::Result<Value> {
    wrapper_value(lua, registry, data)
}
pub fn capture_reference(
    value: &Value,
    registry: &SharedTableRegistry,
) -> mlua::Result<Option<Json>> {
    if let Value::UserData(ud) = value {
        if let Ok(h) = ud.borrow::<SharedTableHandle>() {
            if !registry.contains(&h.data) {
                return Err(err("SharedTable belongs to a different session pool"));
            }
            return Ok(Some(json!({"$shared_table_ref":h.data.id})));
        }
    }
    Ok(None)
}
pub fn push_reference(
    lua: &Lua,
    j: &Json,
    registry: &SharedTableRegistry,
) -> mlua::Result<Option<Value>> {
    if j.get("$shared_table_ref").is_some() && j.as_object().is_none_or(|o| o.len() != 1) {
        return Err(err("invalid $shared_table_ref"));
    }
    let Some(id) = j.get("$shared_table_ref").and_then(Json::as_u64) else {
        return Ok(None);
    };
    let data = registry
        .find(id)
        .ok_or_else(|| err("SharedTable reference is outside this session pool"))?;
    Ok(Some(wrapper_value(lua, registry, data)?))
}

#[cfg(test)]
#[path = "shared_tables_tests.rs"]
mod tests;
