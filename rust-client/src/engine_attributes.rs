//! Rust-owned Instance attribute maps with snapshot awareness and Actor-safe COW.
//!
//! Lua conversion and signal delivery remain host responsibilities. This module
//! owns the values and defines the validation/transaction rules used by those
//! adapters. `Arc` clones are cheap snapshots; the first successful mutation
//! gets a private map.
use serde_json::{Map, Value};
use std::sync::{Arc, OnceLock};
use std::{
    ffi::c_void,
    panic::{catch_unwind, AssertUnwindSafe},
    ptr, slice,
};

pub const MAX_ATTRIBUTES: usize = 1024;

#[derive(Clone, Debug, PartialEq)]
pub struct AttributeStore {
    values: Arc<Map<String, Value>>,
    known: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttributeError {
    Unknown,
    InvalidName,
    UnsupportedValue,
    TooMany,
    NotObject,
}

impl std::fmt::Display for AttributeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unknown => "attribute data is unavailable in this received snapshot",
            Self::InvalidName => "invalid or reserved attribute name",
            Self::UnsupportedValue => "unsupported attribute value type",
            Self::TooMany => "attribute count limit exceeded",
            Self::NotObject => "attribute snapshot must be an object",
        })
    }
}

impl AttributeStore {
    pub fn unknown() -> Self {
        Self {
            values: empty_map(),
            known: false,
        }
    }
    pub fn known_empty() -> Self {
        Self {
            values: empty_map(),
            known: true,
        }
    }
    pub fn from_snapshot(value: Value, known: bool) -> Result<Self, AttributeError> {
        let Value::Object(values) = value else {
            return Err(AttributeError::NotObject);
        };
        Ok(Self {
            values: if values.is_empty() {
                empty_map()
            } else {
                Arc::new(values)
            },
            known,
        })
    }
    pub fn is_known(&self) -> bool {
        self.known
    }
    pub fn count(&self) -> usize {
        self.values.len()
    }
    pub fn get(&self, name: &str) -> Result<Option<&Value>, AttributeError> {
        Ok(self.values.get(name))
    }
    pub fn all(&self) -> Result<Value, AttributeError> {
        if !self.known {
            return Err(AttributeError::Unknown);
        }
        Ok(Value::Object((*self.values).clone()))
    }
    pub fn snapshot(&self) -> Value {
        Value::Object((*self.values).clone())
    }

    /// Apply local SetAttribute semantics only after every validation succeeds.
    /// `None` represents Lua nil and removes a key only on a known snapshot.
    pub fn set(&mut self, name: &str, value: Option<Value>) -> Result<bool, AttributeError> {
        validate_received_name(name)?;
        if let Some(ref value) = value {
            validate_value(value)?;
        }
        let present = self.values.contains_key(name);
        if !present && (value.is_some() || !self.known) && self.values.len() >= MAX_ATTRIBUTES {
            return Err(AttributeError::TooMany);
        }
        let changed = match (&value, self.values.get(name)) {
            (None, Some(old)) => !old.is_null(),
            (None, None) => !self.known,
            (Some(new), Some(old)) => !attribute_value_eq(new, old),
            (Some(_), None) => true,
        };
        if let Some(value) = value {
            if self.values.get(name).is_some_and(|old| attribute_value_eq(old, &value)) {
                return Ok(false);
            }
            let map = Arc::make_mut(&mut self.values);
            map.insert(name.to_owned(), value);
        } else if self.known {
            // Even a JSON-null key must be physically removed. Its removal is
            // intentionally not considered a value change for signal parity.
            if present {
                Arc::make_mut(&mut self.values).remove(name);
            }
        } else {
            if self.values.get(name) != Some(&Value::Null) {
                Arc::make_mut(&mut self.values).insert(name.to_owned(), Value::Null);
            }
        }
        Ok(changed)
    }

    /// Replace an entire received snapshot transactionally and return names
    /// whose observable value changed. Unknown snapshots retain supplied keys.
    pub fn replace_snapshot(
        &mut self,
        incoming: Value,
        known: bool,
    ) -> Result<Vec<String>, AttributeError> {
        let Value::Object(incoming) = incoming else {
            return Err(AttributeError::NotObject);
        };
        let mut changed = Vec::new();
        for (name, old) in self.values.iter() {
            if !incoming
                .get(name)
                .is_some_and(|new| attribute_value_eq(old, new))
            {
                changed.push(name.clone());
            }
        }
        for name in incoming.keys() {
            if !self.values.contains_key(name) {
                changed.push(name.clone());
            }
        }
        changed.sort();
        self.values = if incoming.is_empty() {
            empty_map()
        } else {
            Arc::new(incoming)
        };
        self.known = known;
        Ok(changed)
    }

    /// A received single-key update. A null value means an explicit removal
    /// for known maps; for unknown maps it is retained as a known null entry.
    pub fn receive_update(&mut self, name: &str, value: Value) -> Result<bool, AttributeError> {
        validate_received_name(name)?;
        let present = self.values.contains_key(name);
        if !present && (value != Value::Null || !self.known) && self.values.len() >= MAX_ATTRIBUTES
        {
            return Err(AttributeError::TooMany);
        }
        let old = self.values.get(name);
        let changed = match old {
            Some(old) => !attribute_value_eq(old, &value),
            None => value != Value::Null || !self.known,
        };
        if value.is_null() && self.known {
            if old.is_some() {
                Arc::make_mut(&mut self.values).remove(name);
            }
        } else {
            if old != Some(&value) {
                Arc::make_mut(&mut self.values).insert(name.to_owned(), value);
            }
        }
        Ok(changed)
    }
}

/// Match nlohmann's JSON numeric equality: integral pairs compare exactly,
/// while a mixed integral/floating pair compares through f64. The comparison
/// recurses through JSON containers used by tagged engine value payloads.
fn attribute_value_eq(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => number_eq(a, b),
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| attribute_value_eq(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter().all(|(key, value)| {
                    b.get(key)
                        .is_some_and(|other| attribute_value_eq(value, other))
                })
        }
        _ => left == right,
    }
}

fn number_eq(left: &serde_json::Number, right: &serde_json::Number) -> bool {
    if let (Some(a), Some(b)) = (left.as_i64(), right.as_i64()) {
        return a == b;
    }
    if let (Some(a), Some(b)) = (left.as_u64(), right.as_u64()) {
        return a == b;
    }
    if let (Some(a), Some(b)) = (left.as_i64(), right.as_u64()) {
        return a >= 0 && (a as u64) == b;
    }
    if let (Some(a), Some(b)) = (left.as_u64(), right.as_i64()) {
        return b >= 0 && a == (b as u64);
    }
    match (left.as_f64(), right.as_f64()) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

pub fn validate_received_name(name: &str) -> Result<(), AttributeError> {
    if name.is_empty() || name.len() > 100 || name.contains('\0') {
        return Err(AttributeError::InvalidName);
    }
    Ok(())
}

fn empty_map() -> Arc<Map<String, Value>> {
    static EMPTY: OnceLock<Arc<Map<String, Value>>> = OnceLock::new();
    EMPTY.get_or_init(|| Arc::new(Map::new())).clone()
}

pub fn validate_name(name: &str) -> Result<(), AttributeError> {
    if name.is_empty()
        || name.len() > 100
        || name.starts_with("RBX")
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'/' | b'-'))
    {
        return Err(AttributeError::InvalidName);
    }
    Ok(())
}

// JSON values are the host's canonical serialized representation. The thin
// Lua adapter is responsible for rejecting unsupported Lua types; valid Roblox
// value types serialize to objects (for example vectors and colors).
fn validate_value(value: &Value) -> Result<(), AttributeError> {
    match value {
        Value::Null
        | Value::Bool(_)
        | Value::Number(_)
        | Value::String(_)
        | Value::Array(_)
        | Value::Object(_) => Ok(()),
    }
}

// Opaque node-owned handles used by the C++ Instance adapter. Every callback
// catches panics and returns an error sentinel; no Rust unwind crosses C ABI.
/// Versioned C ABI. Pointer-taking entries are unsafe to call from Rust: callers
/// must provide live store handles, valid byte spans, and uniquely borrowed
/// mutable handles/output buffers. The C++ RAII adapter satisfies these rules.
#[repr(C)]
pub struct RustAttributeStoreApi {
    pub version: u32,
    pub create_empty: extern "C" fn(u8) -> *mut c_void,
    pub create_json: unsafe extern "C" fn(*const u8, usize, u8) -> *mut c_void,
    pub clone_store: unsafe extern "C" fn(*const c_void) -> *mut c_void,
    pub drop_store: unsafe extern "C" fn(*mut c_void),
    pub known: unsafe extern "C" fn(*const c_void) -> i32,
    pub set_known: unsafe extern "C" fn(*mut c_void, u8) -> i32,
    pub size: unsafe extern "C" fn(*const c_void) -> isize,
    pub contains: unsafe extern "C" fn(*const c_void, *const u8, usize) -> i32,
    pub lookup: unsafe extern "C" fn(*const c_void, *const u8, usize, *mut u8, usize) -> isize,
    pub snapshot: unsafe extern "C" fn(*const c_void, *mut u8, usize) -> isize,
    pub replace: unsafe extern "C" fn(*mut c_void, *const u8, usize, u8) -> i32,
    pub set: unsafe extern "C" fn(*mut c_void, *const u8, usize, *const u8, usize, *mut u8) -> i32,
    pub erase: unsafe extern "C" fn(*mut c_void, *const u8, usize, *mut u8) -> i32,
}

/// # Safety
/// For nonzero `len`, `ptr` must point to `len` readable bytes valid for `'a`.
unsafe fn input<'a>(ptr: *const u8, len: usize) -> Result<&'a [u8], ()> {
    if len == 0 {
        return Ok(&[]);
    }
    if ptr.is_null() {
        return Err(());
    }
    Ok(slice::from_raw_parts(ptr, len))
}
/// # Safety
/// `ptr` must be a live handle created by this module and remain alive for `'a`.
unsafe fn handle<'a>(ptr: *const c_void) -> Result<&'a AttributeStore, ()> {
    if ptr.is_null() {
        return Err(());
    }
    Ok(&*(ptr.cast::<AttributeStore>()))
}
/// # Safety
/// `ptr` must be a live, uniquely borrowed handle created by this module.
unsafe fn handle_mut<'a>(ptr: *mut c_void) -> Result<&'a mut AttributeStore, ()> {
    if ptr.is_null() {
        return Err(());
    }
    Ok(&mut *(ptr.cast::<AttributeStore>()))
}
fn decode_value(bytes: &[u8]) -> Result<Value, ()> {
    serde_json::from_slice(bytes).map_err(|_| ())
}
/// # Safety
/// If `out` is nonnull and has enough capacity, it must be writable for the
/// serialized value's full byte length.
unsafe fn output<T: serde::Serialize + ?Sized>(value: &T, out: *mut u8, capacity: usize) -> isize {
    let Ok(bytes) = serde_json::to_vec(value) else {
        return -1;
    };
    if out.is_null() || capacity < bytes.len() {
        return bytes.len() as isize;
    }
    ptr::copy_nonoverlapping(bytes.as_ptr(), out, bytes.len());
    bytes.len() as isize
}
extern "C" fn cb_create_empty(known: u8) -> *mut c_void {
    catch_unwind(|| {
        Box::into_raw(Box::new(if known != 0 {
            AttributeStore::known_empty()
        } else {
            AttributeStore::unknown()
        }))
        .cast()
    })
    .unwrap_or(ptr::null_mut())
}
/// # Safety
/// `data` must reference `len` readable bytes for the duration of this call.
unsafe extern "C" fn cb_create_json(data: *const u8, len: usize, known: u8) -> *mut c_void {
    catch_unwind(AssertUnwindSafe(|| {
        let value = decode_value(input(data, len)?)?;
        let store = AttributeStore::from_snapshot(value, known != 0).map_err(|_| ())?;
        Ok::<_, ()>(Box::into_raw(Box::new(store)).cast())
    }))
    .ok()
    .and_then(Result::ok)
    .unwrap_or(ptr::null_mut())
}
/// # Safety
/// `store` must be a live handle returned by this API.
unsafe extern "C" fn cb_clone_store(store: *const c_void) -> *mut c_void {
    catch_unwind(AssertUnwindSafe(|| {
        Ok::<_, ()>(Box::into_raw(Box::new(handle(store)?.clone())).cast())
    }))
    .ok()
    .and_then(Result::ok)
    .unwrap_or(ptr::null_mut())
}
/// # Safety
/// `store` must be a live handle returned by this API and not previously dropped.
unsafe extern "C" fn cb_drop_store(store: *mut c_void) {
    if !store.is_null() {
        // SAFETY: guaranteed by this callback's handle ownership contract.
        let _ = catch_unwind(AssertUnwindSafe(|| unsafe {
            drop(Box::from_raw(store.cast::<AttributeStore>()))
        }));
    }
}
/// # Safety
/// `store` must be a live handle returned by this API.
unsafe extern "C" fn cb_known(store: *const c_void) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        handle(store).map(|s| s.is_known() as i32).unwrap_or(-1)
    }))
    .unwrap_or(-1)
}
/// # Safety
/// `store` must be a live, uniquely borrowed handle returned by this API.
unsafe extern "C" fn cb_set_known(store: *mut c_void, known: u8) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        let Ok(s) = handle_mut(store) else {
            return -1;
        };
        s.known = known != 0;
        0
    }))
    .unwrap_or(-1)
}
/// # Safety
/// `store` must be a live handle returned by this API.
unsafe extern "C" fn cb_size(store: *const c_void) -> isize {
    catch_unwind(AssertUnwindSafe(|| {
        handle(store).map(|s| s.count() as isize).unwrap_or(-1)
    }))
    .unwrap_or(-1)
}
/// # Safety
/// `store` must be live and `name` must reference `len` readable bytes.
unsafe extern "C" fn cb_contains(store: *const c_void, name: *const u8, len: usize) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        let Ok(s) = handle(store) else {
            return -1;
        };
        let Ok(name) = std::str::from_utf8(input(name, len).unwrap_or(&[])) else {
            return -1;
        };
        s.values.contains_key(name) as i32
    }))
    .unwrap_or(-1)
}
/// # Safety
/// `store` and `name` must be valid; when provided, `out` must be writable for
/// `cap` bytes. No mutation may race this callback.
unsafe extern "C" fn cb_lookup(
    store: *const c_void,
    name: *const u8,
    len: usize,
    out: *mut u8,
    cap: usize,
) -> isize {
    catch_unwind(AssertUnwindSafe(|| {
        let Ok(s) = handle(store) else {
            return -1;
        };
        let Ok(name) = std::str::from_utf8(input(name, len).unwrap_or(&[])) else {
            return -1;
        };
        match s.get(name) {
            Ok(Some(v)) => output(v, out, cap),
            Ok(None) => 0,
            Err(_) => -1,
        }
    }))
    .unwrap_or(-1)
}
/// # Safety
/// `store` must be live; when provided, `out` must be writable for `cap` bytes.
unsafe extern "C" fn cb_snapshot(store: *const c_void, out: *mut u8, cap: usize) -> isize {
    catch_unwind(AssertUnwindSafe(|| {
        handle(store)
            .map(|s| output(s.values.as_ref(), out, cap))
            .unwrap_or(-1)
    }))
    .unwrap_or(-1)
}
/// # Safety
/// `store` must be live and uniquely borrowed; `data` must reference `len`
/// readable bytes for the duration of this call.
unsafe extern "C" fn cb_replace(store: *mut c_void, data: *const u8, len: usize, known: u8) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        let Ok(s) = handle_mut(store) else {
            return -1;
        };
        let Ok(value) = input(data, len).and_then(decode_value) else {
            return -1;
        };
        match s.replace_snapshot(value, known != 0) {
            Ok(_) => 0,
            Err(_) => -1,
        }
    }))
    .unwrap_or(-1)
}
/// # Safety
/// `store` must be live and uniquely borrowed; all byte spans and `changed`
/// must point to valid memory for this call, with `changed` writable.
unsafe extern "C" fn cb_set(
    store: *mut c_void,
    name: *const u8,
    name_len: usize,
    data: *const u8,
    data_len: usize,
    changed: *mut u8,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if changed.is_null() {
            return -1;
        }
        let Ok(s) = handle_mut(store) else {
            return -1;
        };
        let Ok(name) = std::str::from_utf8(input(name, name_len).unwrap_or(&[])) else {
            return -1;
        };
        let Ok(value) = input(data, data_len).and_then(decode_value) else {
            return -1;
        };
        match s.receive_update(name, value) {
            Ok(c) => {
                unsafe {
                    *changed = c as u8;
                }
                0
            }
            Err(_) => -1,
        }
    }))
    .unwrap_or(-1)
}
/// # Safety
/// `store` must be live and uniquely borrowed; `name` and writable `changed`
/// must remain valid for this call.
unsafe extern "C" fn cb_erase(
    store: *mut c_void,
    name: *const u8,
    name_len: usize,
    changed: *mut u8,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if changed.is_null() {
            return -1;
        }
        let Ok(s) = handle_mut(store) else {
            return -1;
        };
        let Ok(name) = std::str::from_utf8(input(name, name_len).unwrap_or(&[])) else {
            return -1;
        };
        let c = s.values.contains_key(name);
        if c {
            Arc::make_mut(&mut s.values).remove(name);
        }
        unsafe {
            *changed = c as u8;
        }
        0
    }))
    .unwrap_or(-1)
}
pub static RUST_ATTRIBUTE_STORE_API: RustAttributeStoreApi = RustAttributeStoreApi {
    version: 1,
    create_empty: cb_create_empty,
    create_json: cb_create_json,
    clone_store: cb_clone_store,
    drop_store: cb_drop_store,
    known: cb_known,
    set_known: cb_set_known,
    size: cb_size,
    contains: cb_contains,
    lookup: cb_lookup,
    snapshot: cb_snapshot,
    replace: cb_replace,
    set: cb_set,
    erase: cb_erase,
};

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_unknown_known_empty_and_reads() {
        let unknown = AttributeStore::unknown();
        assert_eq!(unknown.get("x"), Ok(None));
        assert!(!unknown.is_known());
        assert_eq!(AttributeStore::known_empty().get("x"), Ok(None));
        let another = AttributeStore::known_empty();
        assert!(Arc::ptr_eq(&unknown.values, &another.values));
    }
    #[test]
    fn snapshots_are_actor_isolated_and_mutation_is_transactional() {
        let mut root = AttributeStore::known_empty();
        root.set("x", Some(Value::from(1))).unwrap();
        let mut actor = root.clone();
        assert!(Arc::ptr_eq(&root.values, &actor.values));
        assert!(!actor
            .receive_update("x", Value::from(1))
            .expect("repeated value is valid"));
        assert!(Arc::ptr_eq(&root.values, &actor.values));
        assert!(actor.set("x", Some(Value::from(2))).unwrap());
        assert_eq!(root.get("x").unwrap(), Some(&Value::from(1)));
        let before = actor.clone();
        assert_eq!(
            actor.set("bad\0name", Some(Value::from(3))),
            Err(AttributeError::InvalidName)
        );
        assert_eq!(actor, before);
    }
    #[test]
    fn nil_and_received_null_keep_distinct_known_semantics() {
        let mut unknown = AttributeStore::unknown();
        assert!(unknown.set("x", None).unwrap());
        assert_eq!(unknown.get("x").unwrap(), Some(&Value::Null));
        assert!(!unknown.set("x", None).unwrap());
        let mut known =
            AttributeStore::from_snapshot(serde_json::json!({"RBXInternal": 4}), true).unwrap();
        assert!(!known.receive_update("missing", Value::Null).unwrap());
        assert_eq!(known.get("RBXInternal").unwrap(), Some(&Value::from(4)));
    }
    #[test]
    fn opaque_api_null_and_raw_erase_semantics() {
        let api = &RUST_ATTRIBUTE_STORE_API;
        // SAFETY: handles are returned by this API and each input/output buffer
        // remains live for the duration of its callback; each handle is dropped once.
        unsafe {
            let unknown = (api.create_empty)(0);
            assert!(!unknown.is_null());
            assert_eq!(
                (api.set)(
                    unknown,
                    b"x".as_ptr(),
                    1,
                    b"null".as_ptr(),
                    4,
                    ptr::null_mut()
                ),
                -1
            );
            assert_eq!((api.contains)(unknown, b"x".as_ptr(), 1), 0);
            let mut changed = 0;
            assert_eq!(
                (api.set)(unknown, b"x".as_ptr(), 1, b"null".as_ptr(), 4, &mut changed),
                0
            );
            assert_eq!(changed, 1);
            assert_eq!(
                (api.set)(unknown, b"x".as_ptr(), 1, b"null".as_ptr(), 4, &mut changed),
                0
            );
            assert_eq!(changed, 0);
            assert_eq!((api.erase)(unknown, b"x".as_ptr(), 1, &mut changed), 0);
            assert_eq!(changed, 1);
            assert_eq!((api.contains)(unknown, b"x".as_ptr(), 1), 0);
            (api.drop_store)(unknown);

            let known = (api.create_empty)(1);
            assert_eq!(
                (api.set)(known, b"x".as_ptr(), 1, b"null".as_ptr(), 4, &mut changed),
                0
            );
            assert_eq!(changed, 0);
            assert_eq!((api.contains)(known, b"x".as_ptr(), 1), 0);
            (api.drop_store)(known);

            let null_snapshot = b"{\"x\":null}";
            let known_null = (api.create_json)(null_snapshot.as_ptr(), null_snapshot.len(), 1);
            assert!(!known_null.is_null());
            assert_eq!(
                (api.set)(
                    known_null,
                    b"x".as_ptr(),
                    1,
                    b"null".as_ptr(),
                    4,
                    &mut changed
                ),
                0
            );
            assert_eq!(changed, 0);
            assert_eq!((api.contains)(known_null, b"x".as_ptr(), 1), 0);
            (api.drop_store)(known_null);
        }
    }
    #[test]
    fn received_replacement_is_atomic_and_reports_changes() {
        let mut attrs = AttributeStore::from_snapshot(serde_json::json!({"a":1}), true).unwrap();
        let original = attrs.clone();
        assert_eq!(
            attrs.replace_snapshot(Value::Array(vec![]), false),
            Err(AttributeError::NotObject)
        );
        assert_eq!(attrs, original);
        assert_eq!(
            attrs
                .replace_snapshot(serde_json::json!({"b":2}), true)
                .unwrap(),
            vec!["a", "b"]
        );
    }
    #[test]
    fn numeric_equality_matches_json_for_nested_attribute_payloads() {
        let mut attrs = AttributeStore::from_snapshot(
            serde_json::json!({"n":1,"tag":{"$vector":[1,2.0]},"large":18446744073709551615u64}),
            true,
        )
        .unwrap();
        let shared = attrs.clone();
        assert!(!attrs.set("n", Some(serde_json::json!(1.0))).unwrap());
        assert!(Arc::ptr_eq(&attrs.values, &shared.values));
        assert!(!attrs.receive_update("n", serde_json::json!(1.0)).unwrap());
        assert!(!Arc::ptr_eq(&attrs.values, &shared.values));
        assert_eq!(attrs.values.get("n").and_then(Value::as_f64), Some(1.0));
        assert_eq!(
            attrs
                .replace_snapshot(
                    serde_json::json!({
                        "n":1.0,
                        "tag":{"$vector":[1.0,2]},
                        "large":18446744073709551615u64
                    }),
                    true,
                )
                .unwrap(),
            Vec::<String>::new()
        );
        assert!(!attribute_value_eq(
            &Value::from(u64::MAX),
            &Value::from(i64::MAX)
        ));
    }
}
